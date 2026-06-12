//! MCP client for harness-rs — see docs/specs/2026-05-31-harness-mcp-client.md

mod client;
mod proxy;

pub use client::McpClient;
pub use proxy::McpProxyTool;

/// Re-export of the `reqwest` version this crate links, so callers building a
/// hardened client for [`McpClient::connect_http_with_client`] use the exact
/// `reqwest::Client` type rmcp expects (no version mismatch). `http` feature only.
#[cfg(feature = "http")]
pub use reqwest;
