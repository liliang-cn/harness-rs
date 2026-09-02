//! Minimal MCP (Model Context Protocol) server.
//!
//! Wraps a `ToolRegistry` and speaks JSON-RPC 2.0 over stdio so any MCP
//! client (Claude Code, Cursor, Codex, etc.) can list and invoke the
//! framework's tools. See <https://modelcontextprotocol.io>.
//!
//! Supported methods:
//! - `initialize`
//! - `tools/list`
//! - `tools/call`
//! - `ping`
//!
//! Methods we don't implement reply with `-32601 Method not found`.

pub mod protocol;
pub mod server;

pub use protocol::*;
pub use server::*;
