#![cfg(feature = "test-server")]

use std::sync::{Arc, Mutex};

use harness_core::{Event, Hook, HookOutcome, Task, ToolResult, World};
use harness_loop::{AgentLoop, Outcome};
use harness_mcp_client::McpClient;
use harness_models::{MockModel, MockResponse};
use serde_json::json;

struct CaptureHook {
    last: Arc<Mutex<Option<ToolResult>>>,
}
impl Hook for CaptureHook {
    fn name(&self) -> &str { "capture" }
    fn matches(&self, ev: &Event<'_>) -> bool { matches!(ev, Event::PostToolUse { .. }) }
    fn fire(&self, ev: &Event<'_>, _world: &mut World) -> HookOutcome {
        if let Event::PostToolUse { result, .. } = ev {
            *self.last.lock().unwrap() = Some((*result).clone());
        }
        HookOutcome::Allow
    }
}

#[tokio::test]
async fn mcp_tool_result_flows_through_the_loop() {
    let bin = env!("CARGO_BIN_EXE_mcp-echo-server");
    let client = McpClient::connect_stdio(bin, &[]).await.unwrap();

    let model = MockModel::new()
        .script(MockResponse::tool_call("echo", json!({ "text": "via loop" })))
        .script(MockResponse::text("done"));

    let captured = Arc::new(Mutex::new(None));
    let mut loop_ = AgentLoop::new(model)
        .with_hook(Arc::new(CaptureHook { last: captured.clone() }));
    for t in client.tools() {
        loop_ = loop_.with_tool(t);
    }

    let mut world = harness_context::default_world(".");
    let outcome = loop_
        .run_with_max_iters(
            Task { description: "echo it".into(), source: None, deadline: None },
            &mut world,
            5,
        )
        .await
        .unwrap();

    assert!(matches!(outcome, Outcome::Done { tools_called: 1, .. }));
    let got = captured.lock().unwrap().clone().expect("no PostToolUse captured");
    assert!(got.ok);
    assert!(serde_json::to_string(&got.content).unwrap().contains("via loop"));
}
