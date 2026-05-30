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
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(3)
            .min(20) as usize;

        let result = if let Some(q) = args
            .get("query")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            match self.store.search(&owner, q, limit).await {
                Ok(hits) => json!({"mode": "discover", "query": q, "count": hits.len(), "results": hits}),
                Err(e) => return Ok(err_result(e)),
            }
        } else if let Some(sid) = args.get("session_id").and_then(|v| v.as_str()) {
            let around = args
                .get("around")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let window = args
                .get("window")
                .and_then(|v| v.as_u64())
                .unwrap_or(5) as usize;
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
        Ok(ToolResult {
            ok: true,
            content: result,
            trace: None,
        })
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
        let hits = self
            .store
            .search(&owner, &query, self.top_k)
            .await
            .unwrap_or_default();
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

#[cfg(test)]
mod tests {
    use super::*;
    use harness_context::{default_world, FileRecall};
    use harness_core::{RecallMessage, SessionMeta};

    fn tmp_root() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "harness-recall-tool-{}-{nanos}-{n}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn tool_discovery_scoped_to_owner() {
        let root = tmp_root();
        let store: Arc<dyn RecallStore> = Arc::new(FileRecall::open(&root).unwrap());
        store
            .ensure_session("alice", "s1", &SessionMeta::new("s1", 1))
            .await
            .unwrap();
        store
            .append(
                "alice",
                "s1",
                &RecallMessage::new("user", "deploy the payment service", 1),
            )
            .await
            .unwrap();

        let tool = SessionSearchTool::new(store.clone());
        let mut world = default_world(".");
        world
            .profile
            .extra
            .insert("recall_owner".into(), serde_json::json!("alice"));
        let out = tool
            .invoke(serde_json::json!({"query": "payment deploy"}), &mut world)
            .await
            .unwrap();
        assert!(out.ok);
        assert_eq!(out.content["count"], 1);

        let mut bob = default_world(".");
        bob.profile
            .extra
            .insert("recall_owner".into(), serde_json::json!("bob"));
        let out2 = tool
            .invoke(serde_json::json!({"query": "payment deploy"}), &mut bob)
            .await
            .unwrap();
        assert_eq!(out2.content["count"], 0);

        let _ = std::fs::remove_dir_all(&root);
    }
}
