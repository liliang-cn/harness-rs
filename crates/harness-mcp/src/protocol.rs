//! JSON-RPC 2.0 message shapes used by MCP.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id:      Option<serde_json::Value>,
    pub method:  String,
    #[serde(default)]
    pub params:  serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id:      serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result:  Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error:   Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code:    i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data:    Option<serde_json::Value>,
}

pub const ERR_PARSE:           i32 = -32700;
pub const ERR_INVALID_REQUEST: i32 = -32600;
pub const ERR_METHOD_NOT_FOUND:i32 = -32601;
pub const ERR_INVALID_PARAMS:  i32 = -32602;
pub const ERR_INTERNAL:        i32 = -32603;

// ---------- MCP-level payloads ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities:     Capabilities,
    #[serde(rename = "serverInfo")]
    pub server_info:      ServerInfo,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub tools: ToolsCapability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts:   Option<PromptsCapability>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolsCapability {
    #[serde(rename = "listChanged", default)]
    pub list_changed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourcesCapability {
    #[serde(rename = "listChanged", default)]
    pub list_changed: bool,
    #[serde(default)]
    pub subscribe:    bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptsCapability {
    #[serde(rename = "listChanged", default)]
    pub list_changed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name:    String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name:        String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsListResult {
    pub tools: Vec<ToolDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
    pub name:      String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolResult {
    pub content: Vec<ContentBlock>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
}

// ============== resources/list, resources/read ==============

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourcesListResult {
    pub resources: Vec<ResourceDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceDescriptor {
    pub uri:         String,
    pub name:        String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type:   Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResourceParams {
    pub uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResourceResult {
    pub contents: Vec<ResourceContent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceContent {
    pub uri:       String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub text:      String,
}

// ============== prompts/list (placeholder — always empty for now) ==============

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptsListResult {
    pub prompts: Vec<PromptDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptDescriptor {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
