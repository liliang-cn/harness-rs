//! The MCP server core: dispatch JSON-RPC requests to the framework.

use crate::protocol::*;
use harness_core::{Action, Tool, World};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

pub struct McpServer {
    tools:   HashMap<String, Arc<dyn Tool>>,
    name:    String,
    version: String,
}

impl McpServer {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            tools:   HashMap::new(),
            name:    name.into(),
            version: version.into(),
        }
    }

    pub fn with_tool(mut self, t: Arc<dyn Tool>) -> Self {
        self.tools.insert(t.name().to_string(), t);
        self
    }

    pub fn with_tools(mut self, ts: Vec<Arc<dyn Tool>>) -> Self {
        for t in ts {
            self.tools.insert(t.name().to_string(), t);
        }
        self
    }

    /// Serve over stdio. Reads one JSON-RPC request per line, writes one
    /// response per line. Returns when stdin closes.
    pub async fn serve_stdio(self, world: &mut World) -> anyhow::Result<()> {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let mut reader = tokio::io::BufReader::new(stdin).lines();
        let mut writer = stdout;

        while let Some(line) = reader.next_line().await? {
            if line.trim().is_empty() { continue; }
            let resp = self.handle_line(&line, world).await;
            let json = serde_json::to_string(&resp)?;
            writer.write_all(json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }
        Ok(())
    }

    /// Handle a single JSON-RPC line, suitable for unit tests.
    pub async fn handle_line(&self, line: &str, world: &mut World) -> JsonRpcResponse {
        let req: JsonRpcRequest = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                return error_response(serde_json::Value::Null, ERR_PARSE, format!("parse error: {e}"));
            }
        };
        if req.jsonrpc != "2.0" {
            return error_response(
                req.id.unwrap_or(serde_json::Value::Null),
                ERR_INVALID_REQUEST,
                "jsonrpc must be \"2.0\"".into(),
            );
        }
        let id = req.id.unwrap_or(serde_json::Value::Null);

        match req.method.as_str() {
            "initialize" => self.handle_initialize(id),
            "ping"       => ok_response(id, serde_json::json!({})),
            "tools/list" => self.handle_tools_list(id),
            "tools/call" => self.handle_tools_call(id, req.params, world).await,
            other => error_response(
                id,
                ERR_METHOD_NOT_FOUND,
                format!("method `{other}` not found"),
            ),
        }
    }

    fn handle_initialize(&self, id: serde_json::Value) -> JsonRpcResponse {
        let result = InitializeResult {
            protocol_version: "2025-06-18".into(),
            capabilities: Capabilities {
                tools: ToolsCapability { list_changed: false },
            },
            server_info: ServerInfo {
                name:    self.name.clone(),
                version: self.version.clone(),
            },
        };
        ok_response(id, serde_json::to_value(result).unwrap())
    }

    fn handle_tools_list(&self, id: serde_json::Value) -> JsonRpcResponse {
        let mut tools: Vec<ToolDescriptor> = self
            .tools
            .values()
            .map(|t| {
                let s = t.schema();
                ToolDescriptor {
                    name:         s.name.clone(),
                    description:  s.description.clone(),
                    input_schema: s.input.clone(),
                }
            })
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        ok_response(id, serde_json::to_value(ToolsListResult { tools }).unwrap())
    }

    async fn handle_tools_call(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        world: &mut World,
    ) -> JsonRpcResponse {
        let p: CallToolParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return error_response(id, ERR_INVALID_PARAMS, e.to_string()),
        };
        let Some(tool) = self.tools.get(&p.name).cloned() else {
            return error_response(id, ERR_METHOD_NOT_FOUND, format!("unknown tool: {}", p.name));
        };
        let action = Action {
            tool:    p.name.clone(),
            call_id: format!("mcp_{}_{}", p.name, world.clock.now_ms()),
            args:    p.arguments.clone(),
        };
        match tool.invoke(action.args.clone(), world).await {
            Ok(r) => {
                let text = match &r.content {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let result = CallToolResult {
                    content: vec![ContentBlock::Text { text }],
                    is_error: !r.ok,
                };
                ok_response(id, serde_json::to_value(result).unwrap())
            }
            Err(e) => {
                let result = CallToolResult {
                    content: vec![ContentBlock::Text { text: e.to_string() }],
                    is_error: true,
                };
                ok_response(id, serde_json::to_value(result).unwrap())
            }
        }
    }
}

fn ok_response(id: serde_json::Value, result: serde_json::Value) -> JsonRpcResponse {
    JsonRpcResponse { jsonrpc: "2.0".into(), id, result: Some(result), error: None }
}

fn error_response(id: serde_json::Value, code: i32, message: String) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(JsonRpcError { code, message, data: None }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_context::default_world;
    use harness_tools_fs::{ListDir, ReadFile};

    fn srv() -> McpServer {
        McpServer::new("harness-mcp-test", "0.0.1")
            .with_tool(Arc::new(ListDir))
            .with_tool(Arc::new(ReadFile))
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        let mut world = default_world(".");
        let s = srv();
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let resp = s.handle_line(req, &mut world).await;
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["serverInfo"]["name"], "harness-mcp-test");
    }

    #[tokio::test]
    async fn tools_list_returns_registered_tools() {
        let mut world = default_world(".");
        let s = srv();
        let req = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let resp = s.handle_line(req, &mut world).await;
        let names: Vec<String> = resp.result.unwrap()["tools"]
            .as_array().unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"list_dir".to_string()));
        assert!(names.contains(&"read_file".to_string()));
    }

    #[tokio::test]
    async fn tools_call_dispatches_to_underlying_tool() {
        let td = std::env::temp_dir().join(format!(
            "harness-mcp-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&td).unwrap();
        std::fs::write(td.join("a.txt"), "hello").unwrap();
        let mut world = default_world(td.clone());

        let s = srv();
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"read_file","arguments":{{"path":"a.txt"}}}}}}"#
        );
        let resp = s.handle_line(&req, &mut world).await;
        assert!(resp.error.is_none(), "{:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("hello"), "tool result text: {text}");
        let _ = std::fs::remove_dir_all(&td);
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let mut world = default_world(".");
        let s = srv();
        let req = r#"{"jsonrpc":"2.0","id":4,"method":"banana/peel"}"#;
        let resp = s.handle_line(req, &mut world).await;
        assert_eq!(resp.error.as_ref().unwrap().code, ERR_METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn malformed_json_returns_parse_error() {
        let mut world = default_world(".");
        let s = srv();
        let resp = s.handle_line("{not json", &mut world).await;
        assert_eq!(resp.error.as_ref().unwrap().code, ERR_PARSE);
    }

    #[tokio::test]
    async fn missing_tool_returns_error() {
        let mut world = default_world(".");
        let s = srv();
        let req = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nonexistent","arguments":{}}}"#;
        let resp = s.handle_line(req, &mut world).await;
        assert_eq!(resp.error.as_ref().unwrap().code, ERR_METHOD_NOT_FOUND);
    }
}
