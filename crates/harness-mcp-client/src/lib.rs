//! MCP client for harness-rs — see docs/specs/2026-05-31-harness-mcp-client.md

mod client;
mod proxy;

pub use client::McpClient;
pub use proxy::McpProxyTool;
