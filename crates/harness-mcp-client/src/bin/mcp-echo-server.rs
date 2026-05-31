//! Test-only MCP stdio server exposing one `echo` tool. Used by integration
//! tests via CARGO_BIN_EXE_mcp-echo-server.
use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde_json::{json, Value};
use std::sync::Arc;

struct EchoTool {
    schema: ToolSchema,
}

impl EchoTool {
    fn new() -> Self {
        Self {
            schema: ToolSchema {
                name: "echo".into(),
                description: "echoes back the `text` argument".into(),
                input: json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str { &self.schema.name }
    fn schema(&self) -> &ToolSchema { &self.schema }
    fn risk(&self) -> ToolRisk { ToolRisk::ReadOnly }
    async fn invoke(&self, args: Value, _world: &mut World) -> Result<ToolResult, ToolError> {
        let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
        Ok(ToolResult {
            ok: true,
            content: json!({ "echo": text }),
            trace: Some(format!("echo: {text}")),
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut world = harness_context::default_world(".");
    harness_mcp::McpServer::new("echo-test", "0.1.0")
        .with_tool(Arc::new(EchoTool::new()))
        .serve_stdio(&mut world)
        .await
}
