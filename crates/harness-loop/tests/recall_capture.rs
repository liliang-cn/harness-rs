//! with_recall captures user/assistant/tool messages under owner+session from profile.extra.

use async_trait::async_trait;
use harness_context::{default_world, FileRecall};
use harness_core::{
    Context, Model, ModelError, ModelInfo, ModelOutput, RecallStore, StopReason, Task, ToolCall,
    ToolError, ToolResult, ToolRisk, ToolSchema, Usage, World,
};
use harness_loop::AgentLoop;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

struct MockModel {
    turn: AtomicU32,
}

#[async_trait]
impl Model for MockModel {
    fn info(&self) -> ModelInfo {
        ModelInfo {
            handle: "mock".into(),
            provider: "test".into(),
            model: "mock".into(),
            context_window: 4096,
            input_cost_usd_per_million_tokens: None,
            output_cost_usd_per_million_tokens: None,
            supports_tool_use: true,
            supports_streaming: false,
        }
    }

    async fn complete(&self, _ctx: &Context) -> Result<ModelOutput, ModelError> {
        let t = self.turn.fetch_add(1, Ordering::SeqCst);
        if t == 0 {
            Ok(ModelOutput {
                text: Some("calling tool".into()),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "noop".into(),
                    args: serde_json::json!({}),
                }],
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

struct Noop;

#[async_trait]
impl harness_core::Tool for Noop {
    fn name(&self) -> &str {
        "noop"
    }
    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            name: "noop".into(),
            description: "Does nothing.".into(),
            input: serde_json::json!({"type": "object", "properties": {}}),
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(&self, _args: serde_json::Value, _world: &mut World) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            ok: true,
            content: serde_json::json!({}),
            trace: None,
        })
    }
}

fn tmp_root() -> std::path::PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "harness-recall-cap-{}-{nanos}-{n}",
        std::process::id()
    ))
}

#[tokio::test]
async fn with_recall_captures_the_conversation() {
    let root = tmp_root();
    let store: Arc<dyn RecallStore> = Arc::new(FileRecall::open(&root).unwrap());
    let loop_ = AgentLoop::new(MockModel {
        turn: AtomicU32::new(0),
    })
    .with_recall(store.clone())
    .with_tool(Arc::new(Noop));

    let mut world = default_world(".");
    world
        .profile
        .extra
        .insert("recall_owner".into(), serde_json::json!("u9"));
    world
        .profile
        .extra
        .insert("recall_session".into(), serde_json::json!("conv1"));

    let task = Task {
        description: "remember the alpha protocol".into(),
        source: None,
        deadline: None,
    };
    let _ = loop_.run(task, &mut world).await.unwrap();

    let hits = store.search("u9", "alpha protocol", 5).await.unwrap();
    assert_eq!(hits.len(), 1, "user message should be searchable");
    let scrolled = store.scroll("u9", "conv1", 1, 50).await.unwrap();
    let roles: Vec<&str> = scrolled.iter().map(|m| m.role.as_str()).collect();
    assert!(roles.contains(&"user"));
    assert!(roles.contains(&"assistant"));
    assert!(roles.contains(&"tool"));

    let _ = std::fs::remove_dir_all(&root);
}
