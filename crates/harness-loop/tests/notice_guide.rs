//! A notice that arrives while a run is busy reaches the model on its very
//! next call — appended after everything already said, never spliced in.

use async_trait::async_trait;
use harness_context::default_world;
use harness_core::{
    Context, Model, ModelError, ModelInfo, ModelOutput, Task, Tool, ToolError, ToolResult,
    ToolRisk, ToolSchema, World,
};
use harness_loop::{AgentLoop, NoticeGuide, Outcome};
use harness_models::{MockModel, MockResponse};
use serde_json::{Value, json};
use std::sync::Arc;

struct Shared(Arc<MockModel>);

#[async_trait]
impl Model for Shared {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        self.0.complete(ctx).await
    }
    fn info(&self) -> ModelInfo {
        self.0.info()
    }
}

/// A tool whose side effect is "a job finished while this was running".
struct Slow {
    guide: Arc<NoticeGuide>,
    schema: ToolSchema,
}

#[async_trait]
impl Tool for Slow {
    fn name(&self) -> &str {
        "slow"
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(&self, _args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
        self.guide
            .push("[background job notice] job 3 (gh) exited 0 after 21m.");
        Ok(ToolResult {
            ok: true,
            content: json!("ok"),
            trace: None,
        })
    }
}

#[tokio::test]
async fn a_notice_pushed_mid_run_is_appended_before_the_next_model_call() {
    let guide = NoticeGuide::new();
    let model = Arc::new(
        MockModel::new()
            .script(MockResponse::tool_call("slow", json!({})))
            .script(MockResponse::text("job 3 finished")),
    );
    let tool = Slow {
        guide: guide.clone(),
        schema: ToolSchema {
            name: "slow".into(),
            description: "test tool".into(),
            input: json!({"type": "object"}),
        },
    };
    let mut world = default_world(".");
    let outcome = AgentLoop::new(Shared(model.clone()))
        .with_tool(Arc::new(tool))
        .with_guide(guide.clone())
        .run_with_max_iters(
            Task {
                description: "wait for it".into(),
                source: None,
                deadline: None,
            },
            &mut world,
            5,
        )
        .await
        .unwrap();
    assert!(matches!(outcome, Outcome::Done { .. }));

    let calls = model.calls();
    assert_eq!(calls.len(), 2);
    let first = &calls[0].history_summary;
    assert!(
        !first
            .iter()
            .any(|t| t.texts.iter().any(|x| x.contains("job 3"))),
        "the notice did not exist yet on call 1"
    );
    let second = &calls[1].history_summary;
    let last = second.last().unwrap();
    assert_eq!(last.role, "user");
    assert!(last.texts.iter().any(|t| t.contains("job 3 (gh) exited 0")));
    // Append only: call 1's history is a prefix of call 2's.
    assert!(second.len() > first.len());
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!((&a.role, &a.texts), (&b.role, &b.texts));
    }
    assert!(!guide.has_pending());
    assert_eq!(guide.take_delivered().len(), 1);
}
