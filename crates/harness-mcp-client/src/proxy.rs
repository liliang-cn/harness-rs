use async_trait::async_trait;
use harness_core::{ToolError, ToolResult, ToolRisk, ToolSchema, World};
use harness_core::tool::Tool;
use rmcp::model::CallToolRequestParams;
use rmcp::service::Peer;
use rmcp::RoleClient;

pub(crate) fn build_schema(tool: &rmcp::model::Tool) -> ToolSchema {
    ToolSchema {
        name: tool.name.to_string(),
        description: tool.description.as_deref().unwrap_or("").to_string(),
        input: serde_json::Value::Object((*tool.input_schema).clone()),
    }
}

pub struct McpProxyTool {
    schema: ToolSchema,
    risk: ToolRisk,
    peer: Peer<RoleClient>,
    remote_name: String,
}

impl McpProxyTool {
    pub(crate) fn new(tool: &rmcp::model::Tool, peer: Peer<RoleClient>, risk: ToolRisk) -> Self {
        let schema = build_schema(tool);
        let remote_name = tool.name.to_string();
        Self { schema, risk, peer, remote_name }
    }
}

#[async_trait]
impl Tool for McpProxyTool {
    fn name(&self) -> &str {
        &self.schema.name
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn risk(&self) -> ToolRisk {
        self.risk
    }

    async fn invoke(
        &self,
        args: serde_json::Value,
        _world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let mut params = CallToolRequestParams::new(self.remote_name.clone());
        params.arguments = args.as_object().cloned();

        let res = self.peer.call_tool(params).await.map_err(|e| {
            ToolError::Exec(format!("mcp tool `{}` failed: {e}", self.remote_name))
        })?;

        let text = res
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");

        let ok = !res.is_error.unwrap_or(false);
        let content = res
            .structured_content
            .unwrap_or_else(|| serde_json::json!({"text": text}));

        Ok(ToolResult {
            ok,
            content,
            trace: Some(text),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn make_tool_with_desc(description: Option<&'static str>) -> rmcp::model::Tool {
        let input_schema: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "type": "object",
                "properties": {"q": {"type": "string"}},
                "required": ["q"]
            }))
            .unwrap();

        match description {
            Some(desc) => rmcp::model::Tool::new(
                "graphrag_search",
                desc,
                Arc::new(input_schema),
            ),
            None => rmcp::model::Tool::new_with_raw(
                "graphrag_search",
                None,
                Arc::new(input_schema),
            ),
        }
    }

    #[test]
    fn schema_translates_from_rmcp_tool() {
        let tool = make_tool_with_desc(Some("search the graph"));
        let s = build_schema(&tool);

        assert_eq!(s.name, "graphrag_search");
        assert_eq!(s.description, "search the graph");
        assert_eq!(s.input["properties"]["q"]["type"], "string");
    }

    #[test]
    fn missing_description_becomes_empty() {
        let tool = make_tool_with_desc(None);
        let s = build_schema(&tool);

        assert_eq!(s.description, "");
    }
}
