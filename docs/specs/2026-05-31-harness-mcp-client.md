# harness-mcp-client — design spec

**Status:** approved (brainstorm) · **Date:** 2026-05-31

A new workspace crate `harness-mcp-client`: a generic **MCP client** that connects
to an MCP server over stdio and exposes the server's remote tools as harness
`Arc<dyn Tool>`, so any `AgentLoop` can consume any MCP server. It is the
client-side complement to the existing `harness-mcp` crate (which is server-only —
it exposes harness tools *to* external MCP clients; it cannot connect *out*).

**First consumer:** CortexDB's MCP stdio server (`importflow_run`, GraphRAG
ingest/search) — the RAG/GraphRAG engine. But the crate is CortexDB-agnostic:
point it at any MCP server.

Built on the official Rust MCP SDK **`rmcp` 1.7.0** (tokio), features `client`,
`transport-child-process`, `transport-io`.

## Why this shape

The whole point of proxying each remote tool as a harness `Tool` is that **the MCP
result re-enters the AgentLoop through the standard path** — `Tool::invoke ->
ToolResult ->` loop dispatch. That means a remote MCP call:
- fires `PreToolUse` / `PostToolUse` (so the permission gate and any UI-forward
  hook see it exactly like a native tool),
- is recorded in the session transcript,
- is fed back into the model context as the tool-result turn.

No side channel. Any usage/cost the server reports (via `CallToolResult.meta` or
`structured_content`) is preserved into the harness `ToolResult` (`content` +
`trace`) so it is visible in the loop's record; surfacing it into the loop's
aggregate `Usage` accounting is a future loop-side change, out of scope here.

## Architecture

```
  AgentLoop ──PreToolUse──▶ permission hook
      │ dispatch
      ▼
  McpProxyTool::invoke(args)            (impl harness_core::Tool)
      │  build CallToolRequestParams { name, arguments }
      ▼
  Peer<RoleClient>::call_tool(params).await        (rmcp)
      │  ⇅ JSON-RPC over stdio
      ▼
  child MCP server process (e.g. `cortexdb mcp`)
      ▲
      └── McpClient owns RunningService<RoleClient,()> (keeps the child alive)
```

- **`McpClient`** owns the live `RunningService<RoleClient, ()>` (its `Drop`
  cancels the session, so it must outlive every proxy tool). It connects by
  spawning a child process via `rmcp::transport::TokioChildProcess`, then
  `().serve(transport).await`, then `list_all_tools()`.
- **`McpProxyTool`** holds a cloned `Peer<RoleClient>` (Send+Sync+Clone+'static) +
  a cached harness `ToolSchema` translated from the rmcp `Tool`. One per remote
  tool. `invoke` calls `peer.call_tool(..)` and maps the result.

## Public API

```rust
pub struct McpClient { /* owns RunningService<RoleClient,()> + Peer */ }

impl McpClient {
    /// Spawn `program args...` as an MCP stdio server and initialize a session.
    pub async fn connect_stdio(program: &str, args: &[&str]) -> anyhow::Result<Self>;

    /// The remote tools, each wrapped as a harness Tool. Default risk =
    /// Destructive (remote, may mutate external state); names in `read_only`
    /// are marked ReadOnly so a Plan-mode permission gate lets them through.
    pub fn tools(&self) -> Vec<Arc<dyn harness_core::Tool>>;
    pub fn tools_with_read_only(&self, read_only: &[&str]) -> Vec<Arc<dyn harness_core::Tool>>;

    /// Remote tool names discovered at connect time.
    pub fn tool_names(&self) -> Vec<String>;
}
```

Wiring (consumer side):

```rust
let mcp = McpClient::connect_stdio("cortexdb", &["mcp"]).await?;
let mut loop_ = AgentLoop::new(model);
for t in mcp.tools_with_read_only(&["graphrag_search"]) {
    loop_ = loop_.with_tool(t);
}
// keep `mcp` alive for the duration of the run
```

## Type mapping (rmcp 1.7.0 → harness)

| rmcp | harness |
|---|---|
| `model::Tool { name: Cow, description: Option<Cow>, input_schema: Arc<JsonObject> }` | `ToolSchema { name: String, description: String, input: Value::Object(map) }` |
| `CallToolRequestParams::new(name)` + `.arguments = args.as_object().cloned()` | built from `Tool::invoke` args `Value` |
| `CallToolResult { content: Vec<Content>, structured_content, is_error }` | `ToolResult { ok: !is_error, content: structured_content ⟶ or {"text": joined}, trace: Some(joined_text) }` |
| `Content`→`RawContent::as_text()→RawTextContent.text` | joined into the text body |

## Error handling

- Connect failure (binary missing, init handshake fails) → `connect_stdio` returns
  `Err` with the program name; the consumer decides (skip MCP / abort).
- A `call_tool` transport/protocol error → `ToolError::Exec` (the loop sees a
  failed tool result and can react; the session stays alive).
- `CallToolResult.is_error == Some(true)` → harness `ToolResult { ok: false, .. }`
  carrying the server's error content.

## Testing

- **Unit:** `McpProxyTool` schema translation from a synthetic `rmcp::model::Tool`
  (name/description/input round-trip; missing description → empty string).
- **Integration (no network, deterministic):** a `[[bin]]` `mcp-echo-server` built
  from `harness-mcp`'s `McpServer` exposing one `echo` tool over stdio.
  `McpClient::connect_stdio(env!("CARGO_BIN_EXE_mcp-echo-server"), &[])` →
  `tool_names()` contains `echo` → invoke the proxied `echo` → assert the
  `ToolResult` echoes the payload.
- **AgentLoop integration (proves "result in the loop"):** build an `AgentLoop`
  with a `MockModel` scripted to call `echo`, register the proxied tool, attach a
  capture hook on `PostToolUse`, run, and assert the captured tool result contains
  the echoed payload and `Outcome::Done { tools_called: 1, .. }`.

## Non-goals (v1)

HTTP / SSE / streamable-http transports (rmcp supports them; add later behind
features), MCP auth, resources/prompts (only `tools/*`), the CortexDB-specific
`KnowledgeGuide` for auto-injecting retrieved context, and the liteparse
front-door — all separate, additive work. This crate is transport=stdio,
capability=tools only.

## Dependencies

`rmcp = { version = "1.7", features = ["client", "transport-child-process", "transport-io"] }`,
`harness-core`, `async-trait`, `tokio`, `serde_json`, `anyhow`, `tracing`.
Dev: `harness-mcp`, `harness-context`, `harness-loop`, `harness-models`.
