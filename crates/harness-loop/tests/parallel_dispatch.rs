//! Parallel read-only dispatch: when one model response emits several read-only
//! tool calls, the loop dispatches them concurrently (a mutating call would be a
//! serial barrier). We prove it by making the tool *slow* — 3 × 150ms run in
//! ~150ms wall-clock, not ~450ms.

use async_trait::async_trait;
use harness_core::{
    Context, Model, ModelError, ModelInfo, ModelOutput, StopReason, Tool, ToolCall, ToolError,
    ToolResult, ToolRisk, ToolSchema, Usage, World,
};
use harness_loop::{AgentLoop, Outcome};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

fn mi() -> ModelInfo {
    ModelInfo {
        handle: "mock".into(),
        provider: "mock".into(),
        model: "mock".into(),
        context_window: 8192,
        input_cost_usd_per_million_tokens: None,
        output_cost_usd_per_million_tokens: None,
        supports_tool_use: true,
        supports_streaming: false,
    }
}

/// A read-only tool that sleeps, so concurrency shows up as wall-clock savings.
struct SlowRead {
    schema: ToolSchema,
}
impl SlowRead {
    fn new() -> Self {
        Self {
            schema: ToolSchema {
                name: "slow_read".into(),
                description: "sleeps then echoes".into(),
                input: json!({"type": "object"}),
            },
        }
    }
}
#[async_trait]
impl Tool for SlowRead {
    fn name(&self) -> &str {
        "slow_read"
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(
        &self,
        args: serde_json::Value,
        _w: &mut World,
    ) -> Result<ToolResult, ToolError> {
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(ToolResult {
            ok: true,
            content: json!({ "echo": args["n"] }),
            trace: None,
        })
    }
}

/// One response with three read-only tool calls, then a terminal text turn.
struct ThreeReads {
    turn: AtomicU32,
}
#[async_trait]
impl Model for ThreeReads {
    async fn complete(&self, _c: &Context) -> Result<ModelOutput, ModelError> {
        let t = self.turn.fetch_add(1, Ordering::SeqCst);
        if t == 0 {
            Ok(ModelOutput {
                text: None,
                tool_calls: (0..3)
                    .map(|i| ToolCall {
                        id: format!("c{i}"),
                        name: "slow_read".into(),
                        args: json!({ "n": i }),
                    })
                    .collect(),
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
    fn info(&self) -> ModelInfo {
        mi()
    }
}

#[tokio::test]
async fn read_only_calls_dispatch_concurrently() {
    let mut world = harness_context::default_world(".");
    let loop_ = AgentLoop::new(ThreeReads {
        turn: AtomicU32::new(0),
    })
    .with_tool(Arc::new(SlowRead::new()));
    let task = harness_core::Task {
        description: "go".into(),
        source: None,
        deadline: None,
    };

    let start = Instant::now();
    let out = loop_.run_with_max_iters(task, &mut world, 4).await.unwrap();
    let elapsed = start.elapsed();

    // Sequential would be ~450ms (3 × 150). Concurrent is ~150ms. Assert we're
    // comfortably under the sequential figure.
    assert!(
        elapsed < Duration::from_millis(350),
        "read-only tools did not run concurrently — took {elapsed:?}"
    );
    match out {
        Outcome::Done { tools_called, .. } => assert_eq!(tools_called, 3),
        other => panic!("expected Done, got {other:?}"),
    }
}
