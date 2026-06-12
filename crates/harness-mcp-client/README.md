# harness-rs-mcp-client

Connect a harness `AgentLoop` to any MCP server over stdio. The server's tools
become harness `Arc<dyn Tool>`, so MCP results flow back through the standard
loop path (PreToolUse / PostToolUse, session record, context) — not a side channel.

Built on the official [`rmcp`](https://crates.io/crates/rmcp) SDK. Complements
`harness-rs-mcp` (the server side, which exposes harness tools *to* MCP clients).

## Usage

```rust
use harness_mcp_client::McpClient;
use harness_loop::AgentLoop;

// CortexDB ships a stdio MCP server binary `cortexdb-mcp-stdio` (DB path via the
// CORTEXDB_PATH env var; verified to expose 47 RAG/GraphRAG tools).
let mcp = McpClient::connect_stdio("cortexdb-mcp-stdio", &[]).await?;
let mut loop_ = AgentLoop::new(model);
for t in mcp.tools_with_read_only(&["knowledge_search", "search_text"]) {
    loop_ = loop_.with_tool(t);
}
// keep `mcp` alive for the duration of the run (it owns the child session).
```

Remote tools default to `Destructive` risk (a permission gate should treat them
conservatively); names passed to `tools_with_read_only` are marked `ReadOnly`.

To discover what tools a server exposes before wiring them up:

```rust
println!("{:?}", mcp.tool_names());
```

## API

| Method | Description |
|---|---|
| `McpClient::connect_stdio(program, args)` | Spawn an MCP stdio server and initialize a session |
| `McpClient::connect_http(url)` | Connect over Streamable HTTP with a default client (`http` feature, on by default). **Follows redirects — not SSRF-safe for untrusted URLs.** |
| `McpClient::connect_http_with_client(url, client)` | Connect over Streamable HTTP with a caller-supplied `reqwest::Client` — the SSRF-safe entry point |
| `.tools()` | All remote tools as `Arc<dyn Tool>` (all `Destructive`) |
| `.tools_with_read_only(names)` | Same, but listed names are marked `ReadOnly` |
| `.tool_names()` | Names of tools discovered at connect time |

## Scope

Transports supported:

- **stdio** (child process) via `connect_stdio`
- **Streamable HTTP** (MCP 2025-03-26 spec) via `connect_http` — feature `http`, on by default.
  SSE is subsumed by Streamable HTTP in the MCP spec; this transport handles both.

Capability: tools only (no resources/prompts). Auth is future work.

### SSRF safety (untrusted URLs)

`connect_http` uses a default reqwest client that **follows HTTP redirects** and
re-resolves DNS at connect time, so pre-validating the URL does not stop SSRF: a
`302 Location: http://169.254.169.254/…` or DNS rebinding reaches internal
targets. For untrusted input, validate the URL, resolve the host to an
allow-listed IP, and pass your own hardened client:

```rust
use harness_mcp_client::{McpClient, reqwest}; // re-exported, version-matched

let client = reqwest::Client::builder()
    .redirect(reqwest::redirect::Policy::none())        // kill redirect bypass
    .resolve(host, validated_ip_addr)                   // pin host → vetted IP (no rebinding)
    .build()?;
let mcp = McpClient::connect_http_with_client(url, client).await?;
```

The security policy stays on your side; the crate just provides the seam.

Non-text MCP content blocks (image, resource, audio) are omitted from the tool
result with a `tracing::warn!`; text and structured content pass through normally.

See `docs/specs/2026-05-31-harness-mcp-client.md`.
