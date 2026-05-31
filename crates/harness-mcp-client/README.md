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

let mcp = McpClient::connect_stdio("cortexdb", &["mcp"]).await?;
let mut loop_ = AgentLoop::new(model);
for t in mcp.tools_with_read_only(&["graphrag_search"]) {
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
| `.tools()` | All remote tools as `Arc<dyn Tool>` (all `Destructive`) |
| `.tools_with_read_only(names)` | Same, but listed names are marked `ReadOnly` |
| `.tool_names()` | Names of tools discovered at connect time |

## Scope

Transport: stdio (child process). Capability: tools only (no resources/prompts).
HTTP/SSE transports and auth are future work.

Non-text MCP content blocks (image, resource, audio) are omitted from the tool
result with a `tracing::warn!`; text and structured content pass through normally.

See `docs/specs/2026-05-31-harness-mcp-client.md`.
