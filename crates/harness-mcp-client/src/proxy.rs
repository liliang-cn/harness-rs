use async_trait::async_trait;
use harness_core::tool::Tool;
use harness_core::{ToolError, ToolResult, ToolRisk, ToolSchema, World};
use rmcp::RoleClient;
use rmcp::model::{CallToolRequestParams, CallToolResult, RawContent};
use rmcp::service::RunningService;
use std::sync::Arc;

type JsonObject = serde_json::Map<String, serde_json::Value>;

pub(crate) fn build_schema(tool: &rmcp::model::Tool) -> ToolSchema {
    ToolSchema {
        name: tool.name.to_string(),
        description: tool.description.as_deref().unwrap_or("").to_string(),
        input: serde_json::Value::Object((*tool.input_schema).clone()),
    }
}

/// Convert a raw `serde_json::Value` into tool arguments.
///
/// - Object  → `Some(map)` (the normal case)
/// - Null    → `None` (no-arg call)
/// - Anything else → `Err(InvalidArgs)` with `name` set to `tool_name`
pub(crate) fn to_arguments(
    tool_name: &str,
    args: &serde_json::Value,
) -> Result<Option<JsonObject>, ToolError> {
    match args {
        serde_json::Value::Object(map) => Ok(Some(map.clone())),
        serde_json::Value::Null => Ok(None),
        other => Err(ToolError::InvalidArgs {
            name: tool_name.into(),
            reason: format!(
                "tool arguments must be a JSON object, got {}",
                kind_of(other)
            ),
        }),
    }
}

fn kind_of(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Map a `CallToolResult` from the remote MCP server to our `ToolResult`.
///
/// Non-text content blocks (image, resource, audio, resource_link) are
/// logged via `tracing::warn!` so the caller isn't silently misled.
pub(crate) fn map_call_result(res: CallToolResult) -> ToolResult {
    // Collect text and identify skipped (non-text) block kinds.
    let mut texts: Vec<String> = Vec::new();
    let mut omitted: Vec<&'static str> = Vec::new();

    for block in &res.content {
        match &block.raw {
            RawContent::Text(t) => texts.push(t.text.clone()),
            RawContent::Image(_) => {
                if !omitted.contains(&"image") {
                    omitted.push("image");
                }
            }
            RawContent::Resource(_) => {
                if !omitted.contains(&"resource") {
                    omitted.push("resource");
                }
            }
            RawContent::Audio(_) => {
                if !omitted.contains(&"audio") {
                    omitted.push("audio");
                }
            }
            RawContent::ResourceLink(_) => {
                if !omitted.contains(&"resource_link") {
                    omitted.push("resource_link");
                }
            }
        }
    }

    if !omitted.is_empty() {
        tracing::warn!("non-text MCP content blocks omitted: {:?}", omitted);
    }

    let text = texts.join("\n");
    let ok = !res.is_error.unwrap_or(false);

    let content = if let Some(structured) = res.structured_content {
        structured
    } else if !text.is_empty() {
        serde_json::json!({"text": text})
    } else if !omitted.is_empty() {
        serde_json::json!({"text": "", "omitted_content": omitted})
    } else {
        serde_json::json!({"text": ""})
    };

    let trace = if text.is_empty() { None } else { Some(text) };

    ToolResult { ok, content, trace }
}

pub struct McpProxyTool {
    schema: ToolSchema,
    risk: ToolRisk,
    // Holds the running MCP session (the child stdio server) alive for the tool's
    // lifetime — so dropping the originating `McpClient` doesn't break the tool.
    service: Arc<RunningService<RoleClient, ()>>,
}

impl McpProxyTool {
    pub(crate) fn new(
        tool: &rmcp::model::Tool,
        service: Arc<RunningService<RoleClient, ()>>,
        risk: ToolRisk,
    ) -> Self {
        let schema = build_schema(tool);
        Self {
            schema,
            risk,
            service,
        }
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
        let mut params = CallToolRequestParams::new(self.schema.name.clone());
        params.arguments = to_arguments(&self.schema.name, &args)?;

        let res =
            self.service.peer().call_tool(params).await.map_err(|e| {
                ToolError::Exec(format!("mcp tool `{}` failed: {e}", self.schema.name))
            })?;

        Ok(map_call_result(res))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rmcp::model::{CallToolResult, Content};
    use serde_json::json;

    use super::*;

    // ── build_schema tests (existing) ────────────────────────────────────────

    fn make_tool_with_desc(description: Option<&'static str>) -> rmcp::model::Tool {
        let input_schema: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(json!({
                "type": "object",
                "properties": {"q": {"type": "string"}},
                "required": ["q"]
            }))
            .unwrap();

        match description {
            Some(desc) => rmcp::model::Tool::new("graphrag_search", desc, Arc::new(input_schema)),
            None => {
                rmcp::model::Tool::new_with_raw("graphrag_search", None, Arc::new(input_schema))
            }
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

    // ── to_arguments tests ───────────────────────────────────────────────────

    #[test]
    fn to_arguments_object_returns_some_map() {
        let v = json!({"key": "value", "n": 42});
        let result = to_arguments("my_tool", &v).unwrap();
        assert!(result.is_some());
        let map = result.unwrap();
        assert_eq!(map["key"], json!("value"));
        assert_eq!(map["n"], json!(42));
    }

    #[test]
    fn to_arguments_null_returns_none() {
        let result = to_arguments("my_tool", &serde_json::Value::Null).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn to_arguments_string_is_rejected() {
        let err = to_arguments("search", &json!("hello")).unwrap_err();
        match err {
            ToolError::InvalidArgs { name, reason } => {
                assert_eq!(name, "search", "error name should equal the tool name");
                assert!(
                    reason.contains("object"),
                    "reason should mention 'object': {reason}"
                );
                assert!(
                    reason.contains("string"),
                    "reason should mention the actual kind: {reason}"
                );
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[test]
    fn to_arguments_array_is_rejected() {
        let err = to_arguments("list_files", &json!([1, 2, 3])).unwrap_err();
        match err {
            ToolError::InvalidArgs { name, reason } => {
                assert_eq!(name, "list_files", "error name should equal the tool name");
                assert!(
                    reason.contains("object"),
                    "reason should mention 'object': {reason}"
                );
                assert!(
                    reason.contains("array"),
                    "reason should mention the actual kind: {reason}"
                );
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[test]
    fn to_arguments_number_is_rejected() {
        let err = to_arguments("calc", &json!(7)).unwrap_err();
        match err {
            ToolError::InvalidArgs { name, reason } => {
                assert_eq!(name, "calc", "error name should equal the tool name");
                assert!(
                    reason.contains("object"),
                    "reason should mention 'object': {reason}"
                );
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    // ── map_call_result tests ────────────────────────────────────────────────

    #[test]
    fn map_result_text_success() {
        let res = CallToolResult::success(vec![Content::text("hello world")]);
        let r = map_call_result(res);
        assert!(r.ok);
        assert_eq!(r.content["text"], json!("hello world"));
        assert_eq!(r.trace, Some("hello world".into()));
    }

    #[test]
    fn map_result_error_flag_propagated() {
        let res = CallToolResult::error(vec![Content::text("something went wrong")]);
        let r = map_call_result(res);
        assert!(!r.ok);
        assert_eq!(r.content["text"], json!("something went wrong"));
    }

    #[test]
    fn map_result_structured_content_takes_precedence() {
        let structured = json!({"temperature": 22.5, "unit": "C"});
        let res = CallToolResult::structured(structured.clone());
        let r = map_call_result(res);
        assert!(r.ok);
        assert_eq!(r.content, structured);
    }

    #[test]
    fn map_result_non_text_block_yields_omitted_content() {
        // Construct via serde_json since Content::image requires base64 data
        // but we just need a non-text variant — use the JSON representation.
        let res: CallToolResult = serde_json::from_value(json!({
            "content": [
                {"type": "image", "data": "abc123", "mimeType": "image/png"}
            ],
            "isError": false
        }))
        .unwrap();

        let r = map_call_result(res);
        assert!(r.ok);
        // No text, but we should get omitted_content in the content object.
        assert!(
            r.content.get("omitted_content").is_some(),
            "expected omitted_content field, got: {}",
            r.content
        );
    }

    #[test]
    fn map_result_duplicate_non_text_kinds_deduped() {
        // Two image blocks + one resource block → omitted_content should be
        // ["image", "resource"], not ["image", "image", "resource"].
        let res: CallToolResult = serde_json::from_value(json!({
            "content": [
                {"type": "image", "data": "aaa", "mimeType": "image/png"},
                {"type": "image", "data": "bbb", "mimeType": "image/jpeg"},
                {"type": "resource", "resource": {"uri": "file:///x", "mimeType": "text/plain", "text": "hi"}}
            ],
            "isError": false
        }))
        .unwrap();

        let r = map_call_result(res);
        let omitted = r.content["omitted_content"]
            .as_array()
            .expect("omitted_content should be an array");
        assert_eq!(
            omitted,
            &[json!("image"), json!("resource")],
            "duplicate kinds must be collapsed to first-seen order"
        );
    }

    #[test]
    fn map_result_multi_text_joined() {
        let res =
            CallToolResult::success(vec![Content::text("line one"), Content::text("line two")]);
        let r = map_call_result(res);
        assert_eq!(r.content["text"], json!("line one\nline two"));
    }
}
