# harness-mcp-client Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A workspace crate `harness-mcp-client` that connects to an MCP server over stdio and exposes its remote tools as harness `Arc<dyn Tool>`, so any `AgentLoop` consumes any MCP server — with MCP results flowing back through the standard loop path (PreToolUse/PostToolUse, session record, context).

**Architecture:** `McpClient` owns a live `rmcp` `RunningService<RoleClient, ()>` (spawned child process); `McpProxyTool` holds a cloned `Peer<RoleClient>` + a cached harness `ToolSchema` and, on `invoke`, calls `peer.call_tool(..)` and maps `CallToolResult` → harness `ToolResult`.

**Tech Stack:** Rust, tokio, `rmcp` 1.7.0 (features `client`, `transport-child-process`, `transport-io`), `harness-core`; dev: `harness-mcp` (server, for the test echo bin), `harness-loop` + `harness-models` (AgentLoop integration test).

**Spec:** `docs/specs/2026-05-31-harness-mcp-client.md`.

**Verified rmcp 1.7.0 API (from registry source):**
- `rmcp::transport::TokioChildProcess::new(command: impl Into<CommandWrap>) -> io::Result<Self>`; `rmcp::transport::ConfigureCommandExt` adds `Command::configure(|c| {..})`.
- `use rmcp::ServiceExt;` → `().serve(transport).await? -> RunningService<RoleClient, ()>`. `RunningService: Deref<Target = Peer<RoleClient>>`; `.peer() -> &Peer<RoleClient>`; `Peer` is `Clone + Send + Sync + 'static`.
- `Peer<RoleClient>::list_all_tools(&self) -> Result<Vec<rmcp::model::Tool>, ServiceError>`.
- `Peer<RoleClient>::call_tool(&self, params: CallToolRequestParams) -> Result<CallToolResult, ServiceError>`.
- `rmcp::model::Tool { name: Cow<'static,str>, description: Option<Cow<'static,str>>, input_schema: Arc<JsonObject>, .. }`; `JsonObject = serde_json::Map<String, Value>`.
- `CallToolRequestParams` is `#[non_exhaustive]` + `Default`; build via `CallToolRequestParams::new(name)` then set `.arguments: Option<JsonObject>`.
- `CallToolResult { content: Vec<Content>, structured_content: Option<Value>, is_error: Option<bool>, .. }`; `Content` derefs to `RawContent`; `RawContent::as_text() -> Option<&RawTextContent>`; `RawTextContent.text: String`.
- harness `Tool`: `fn name(&self)->&str; fn schema(&self)->&ToolSchema; fn risk(&self)->ToolRisk; async fn invoke(&self, args: Value, &mut World)->Result<ToolResult, ToolError>`. `ToolSchema { name, description, input }`; `ToolResult { ok, content, trace }`; `ToolError::Exec(String)`.
- harness-mcp server: `McpServer::new(name, version).with_tool(Arc<dyn Tool>).serve_stdio(&mut World) -> anyhow::Result<()>`.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/harness-mcp-client/Cargo.toml` | manifest + rmcp/harness deps, the `mcp-echo-server` test bin |
| `crates/harness-mcp-client/src/lib.rs` | exports; `McpClient` connect/tools |
| `crates/harness-mcp-client/src/proxy.rs` | `McpProxyTool` (impl harness `Tool`) + result mapping |
| `crates/harness-mcp-client/src/bin/mcp-echo-server.rs` | test-only MCP stdio server with one `echo` tool |
| `crates/harness-mcp-client/tests/connect.rs` | integration: connect to echo bin, list + call |
| `crates/harness-mcp-client/tests/in_loop.rs` | AgentLoop integration: MockModel calls proxied tool, assert result re-enters loop |
| `Cargo.toml` (workspace) | add the new member |

---

## Task 1: Scaffold the crate

**Files:**
- Modify: `Cargo.toml` (workspace members)
- Create: `crates/harness-mcp-client/Cargo.toml`
- Create: `crates/harness-mcp-client/src/lib.rs`

- [ ] **Step 1: Add the workspace member**

In the root `Cargo.toml`, in `members = [ ... ]`, add after `"crates/harness-mcp",`:

```toml
    "crates/harness-mcp-client",
```

- [ ] **Step 2: Write `crates/harness-mcp-client/Cargo.toml`**

```toml
[package]
name = "harness-rs-mcp-client"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "MCP client for harness-rs: expose a remote MCP server's tools as harness tools"

[lib]
name = "harness_mcp_client"
path = "src/lib.rs"

[[bin]]
name = "mcp-echo-server"
path = "src/bin/mcp-echo-server.rs"

[dependencies]
harness-core = { workspace = true }
rmcp = { version = "1.7", features = ["client", "transport-child-process", "transport-io"] }
async-trait = { workspace = true }
tokio = { workspace = true }
serde_json = { workspace = true }
anyhow = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
harness-mcp = { workspace = true }
harness-context = { workspace = true }
harness-loop = { workspace = true }
harness-models = { workspace = true }
```

> If any of `async-trait`, `tokio`, `serde_json`, `anyhow`, `tracing` are not in the workspace `[workspace.dependencies]`, replace `{ workspace = true }` with a concrete version (match what `crates/harness-mcp/Cargo.toml` uses). Check `crates/harness-mcp/Cargo.toml` for the exact forms.

- [ ] **Step 3: Write a placeholder `src/lib.rs`**

```rust
//! MCP client for harness-rs — see docs/specs/2026-05-31-harness-mcp-client.md
```

- [ ] **Step 4: Create a placeholder bin so the manifest is valid**

`src/bin/mcp-echo-server.rs`:

```rust
fn main() {}
```

- [ ] **Step 5: Build**

Run: `cargo build -p harness-rs-mcp-client`
Expected: compiles (downloads rmcp 1.7.0 + transitive deps).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/harness-mcp-client
git commit -m "feat(mcp-client): scaffold crate with rmcp 1.7 deps"
```

---

## Task 2: McpProxyTool + result mapping

**Files:**
- Create: `crates/harness-mcp-client/src/proxy.rs`
- Modify: `crates/harness-mcp-client/src/lib.rs`

- [ ] **Step 1: Write the failing unit test**

Append to `src/proxy.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Tool as McpTool;
    use serde_json::json;
    use std::sync::Arc;

    fn synthetic_tool() -> McpTool {
        let schema = json!({
            "type": "object",
            "properties": { "q": { "type": "string" } },
            "required": ["q"]
        });
        let obj = schema.as_object().unwrap().clone();
        McpTool {
            name: "graphrag_search".into(),
            title: None,
            description: Some("search the graph".into()),
            input_schema: Arc::new(obj),
            output_schema: None,
            annotations: None,
            icons: None,
        }
    }

    #[test]
    fn schema_translates_from_rmcp_tool() {
        let s = build_schema(&synthetic_tool());
        assert_eq!(s.name, "graphrag_search");
        assert_eq!(s.description, "search the graph");
        assert_eq!(s.input["properties"]["q"]["type"], "string");
    }

    #[test]
    fn missing_description_becomes_empty() {
        let mut t = synthetic_tool();
        t.description = None;
        let s = build_schema(&t);
        assert_eq!(s.description, "");
    }
}
```

> The `McpTool` struct is `#[non_exhaustive]`-tolerant in construction within tests via field list; if extra fields are required by the compiler, add them per the error (rmcp 1.7 fields: `name, title, description, input_schema, output_schema, annotations, icons`). If construction is blocked by `#[non_exhaustive]`, switch the test to deserialize from JSON: `serde_json::from_value::<McpTool>(json!({"name":"graphrag_search","description":"search the graph","inputSchema":{...}})).unwrap()`.

- [ ] **Step 2: Write the implementation above the tests**

```rust
use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use rmcp::model::{CallToolRequestParams, Tool as McpTool};
use rmcp::service::{Peer, RoleClient};
use serde_json::{json, Value};

/// Translate an rmcp tool descriptor into a harness `ToolSchema`.
pub(crate) fn build_schema(tool: &McpTool) -> ToolSchema {
    ToolSchema {
        name: tool.name.to_string(),
        description: tool.description.as_deref().unwrap_or("").to_string(),
        input: Value::Object((*tool.input_schema).clone()),
    }
}

/// A remote MCP tool exposed as a harness `Tool`. Holds a clonable `Peer`
/// to the running MCP client session.
pub struct McpProxyTool {
    schema: ToolSchema,
    risk: ToolRisk,
    peer: Peer<RoleClient>,
    remote_name: String,
}

impl McpProxyTool {
    pub(crate) fn new(tool: &McpTool, peer: Peer<RoleClient>, risk: ToolRisk) -> Self {
        Self {
            schema: build_schema(tool),
            risk,
            peer,
            remote_name: tool.name.to_string(),
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
    async fn invoke(&self, args: Value, _world: &mut World) -> Result<ToolResult, ToolError> {
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
            .unwrap_or_else(|| json!({ "text": text }));
        Ok(ToolResult {
            ok,
            content,
            trace: Some(text),
        })
    }
}
```

- [ ] **Step 3: Export from `src/lib.rs`**

```rust
//! MCP client for harness-rs — see docs/specs/2026-05-31-harness-mcp-client.md

mod proxy;
pub use proxy::McpProxyTool;
```

- [ ] **Step 4: Run the unit tests**

Run: `cargo test -p harness-rs-mcp-client --lib`
Expected: 2 passed. If `McpTool` struct-literal construction fails, apply the JSON-deserialize fallback noted in Step 1.

- [ ] **Step 5: Commit**

```bash
git add crates/harness-mcp-client/src
git commit -m "feat(mcp-client): McpProxyTool + rmcp->harness result mapping"
```

---

## Task 3: McpClient — connect over stdio, expose tools

**Files:**
- Create: `crates/harness-mcp-client/src/client.rs`
- Modify: `crates/harness-mcp-client/src/lib.rs`

- [ ] **Step 1: Write the implementation** (tested end-to-end in Task 5; no isolated unit test — it needs a live server)

`src/client.rs`:

```rust
use crate::proxy::McpProxyTool;
use harness_core::{Tool, ToolRisk};
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use std::sync::Arc;
use tokio::process::Command;

/// A live MCP client session over a spawned child stdio server. Owns the
/// `RunningService` so the child stays alive for as long as this is held.
pub struct McpClient {
    service: RunningService<RoleClient, ()>,
    tools: Vec<rmcp::model::Tool>,
}

impl McpClient {
    /// Spawn `program args...` as an MCP stdio server and initialize a session.
    pub async fn connect_stdio(program: &str, args: &[&str]) -> anyhow::Result<Self> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let transport = TokioChildProcess::new(Command::new(program).configure(|cmd| {
            for a in &owned {
                cmd.arg(a);
            }
        }))?;
        let service = ()
            .serve(transport)
            .await
            .map_err(|e| anyhow::anyhow!("mcp init for `{program}` failed: {e}"))?;
        let tools = service.list_all_tools().await?;
        Ok(Self { service, tools })
    }

    fn peer(&self) -> Peer<RoleClient> {
        self.service.peer().clone()
    }

    /// Remote tool names discovered at connect time.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name.to_string()).collect()
    }

    /// All remote tools as harness tools (default risk Destructive).
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools_with_read_only(&[])
    }

    /// As `tools`, but names in `read_only` are marked `ReadOnly`.
    pub fn tools_with_read_only(&self, read_only: &[&str]) -> Vec<Arc<dyn Tool>> {
        let peer = self.peer();
        self.tools
            .iter()
            .map(|t| {
                let risk = if read_only.contains(&t.name.as_ref()) {
                    ToolRisk::ReadOnly
                } else {
                    ToolRisk::Destructive
                };
                Arc::new(McpProxyTool::new(t, peer.clone(), risk)) as Arc<dyn Tool>
            })
            .collect()
    }
}
```

- [ ] **Step 2: Export from `src/lib.rs`**

```rust
//! MCP client for harness-rs — see docs/specs/2026-05-31-harness-mcp-client.md

mod client;
mod proxy;

pub use client::McpClient;
pub use proxy::McpProxyTool;
```

- [ ] **Step 3: Build**

Run: `cargo build -p harness-rs-mcp-client`
Expected: compiles. If `ConfigureCommandExt` import path differs, the error names the correct path (it is re-exported at `rmcp::transport::ConfigureCommandExt`).

- [ ] **Step 4: Commit**

```bash
git add crates/harness-mcp-client/src
git commit -m "feat(mcp-client): McpClient::connect_stdio + tool proxying"
```

---

## Task 4: Test MCP echo server bin

**Files:**
- Rewrite: `crates/harness-mcp-client/src/bin/mcp-echo-server.rs`

- [ ] **Step 1: Write the echo server**

```rust
//! Test-only MCP stdio server exposing one `echo` tool. Used by integration
//! tests via CARGO_BIN_EXE_mcp-echo-server.
use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde_json::{json, Value};
use std::sync::Arc;

struct EchoTool {
    schema: ToolSchema,
}

impl EchoTool {
    fn new() -> Self {
        Self {
            schema: ToolSchema {
                name: "echo".into(),
                description: "echoes back the `text` argument".into(),
                input: json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(&self, args: Value, _world: &mut World) -> Result<ToolResult, ToolError> {
        let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
        Ok(ToolResult {
            ok: true,
            content: json!({ "echo": text }),
            trace: Some(format!("echo: {text}")),
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut world = harness_context::default_world(".");
    harness_mcp::McpServer::new("echo-test", "0.1.0")
        .with_tool(Arc::new(EchoTool::new()))
        .serve_stdio(&mut world)
        .await
}
```

- [ ] **Step 2: Build the bin**

Run: `cargo build -p harness-rs-mcp-client --bin mcp-echo-server`
Expected: compiles. (Confirms `McpServer::new().with_tool().serve_stdio()` signature.)

- [ ] **Step 3: Commit**

```bash
git add crates/harness-mcp-client/src/bin/mcp-echo-server.rs
git commit -m "test(mcp-client): echo MCP stdio server for integration tests"
```

---

## Task 5: Integration tests (connect + in-loop)

**Files:**
- Create: `crates/harness-mcp-client/tests/connect.rs`
- Create: `crates/harness-mcp-client/tests/in_loop.rs`

- [ ] **Step 1: Write the connect test**

`tests/connect.rs`:

```rust
use harness_mcp_client::McpClient;
use serde_json::json;

#[tokio::test]
async fn connects_lists_and_calls_echo() {
    let bin = env!("CARGO_BIN_EXE_mcp-echo-server");
    let client = McpClient::connect_stdio(bin, &[]).await.unwrap();

    assert!(client.tool_names().contains(&"echo".to_string()));

    let tools = client.tools();
    let echo = tools.iter().find(|t| t.name() == "echo").unwrap();

    let mut world = harness_context::default_world(".");
    let res = echo
        .invoke(json!({ "text": "hello mcp" }), &mut world)
        .await
        .unwrap();

    assert!(res.ok);
    // server returned structured_content {"echo":"hello mcp"} OR text — accept both.
    let body = serde_json::to_string(&res.content).unwrap();
    assert!(body.contains("hello mcp"), "result did not echo payload: {body}");
}
```

Add `harness-context` is already a dev-dependency (Task 1).

- [ ] **Step 2: Run the connect test**

Run: `cargo test -p harness-rs-mcp-client --test connect`
Expected: PASS — proves spawn → initialize → list_all_tools → call_tool → mapped ToolResult.

- [ ] **Step 3: Write the in-loop test (proves the MCP result re-enters the AgentLoop)**

`tests/in_loop.rs`:

```rust
use std::sync::{Arc, Mutex};

use harness_core::{Event, Hook, HookOutcome, Task, ToolResult, World};
use harness_loop::{AgentLoop, Outcome};
use harness_mcp_client::McpClient;
use harness_models::{MockModel, MockResponse};
use serde_json::json;

/// Captures the result of every PostToolUse so the test can assert the MCP
/// tool result actually flowed back through the loop.
struct CaptureHook {
    last: Arc<Mutex<Option<ToolResult>>>,
}
impl Hook for CaptureHook {
    fn name(&self) -> &str {
        "capture"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PostToolUse { .. })
    }
    fn fire(&self, ev: &Event<'_>, _world: &mut World) -> HookOutcome {
        if let Event::PostToolUse { result, .. } = ev {
            *self.last.lock().unwrap() = Some((*result).clone());
        }
        HookOutcome::Allow
    }
}

#[tokio::test]
async fn mcp_tool_result_flows_through_the_loop() {
    let bin = env!("CARGO_BIN_EXE_mcp-echo-server");
    let client = McpClient::connect_stdio(bin, &[]).await.unwrap();

    let model = MockModel::new()
        .script(MockResponse::tool_call("echo", json!({ "text": "via loop" })))
        .script(MockResponse::text("done"));

    let captured = Arc::new(Mutex::new(None));
    let mut loop_ = AgentLoop::new(model)
        .with_hook(Arc::new(CaptureHook { last: captured.clone() }));
    for t in client.tools() {
        loop_ = loop_.with_tool(t);
    }

    let mut world = harness_context::default_world(".");
    let outcome = loop_
        .run_with_max_iters(
            Task { description: "echo it".into(), source: None, deadline: None },
            &mut world,
            5,
        )
        .await
        .unwrap();

    // The MCP tool was dispatched by the loop ...
    assert!(matches!(outcome, Outcome::Done { tools_called: 1, .. }));
    // ... and its result re-entered the loop via PostToolUse.
    let got = captured.lock().unwrap().clone().expect("no PostToolUse captured");
    assert!(got.ok);
    assert!(serde_json::to_string(&got.content).unwrap().contains("via loop"));
}
```

- [ ] **Step 4: Run the in-loop test**

Run: `cargo test -p harness-rs-mcp-client --test in_loop`
Expected: PASS. If `Event::PostToolUse` field names differ, they are `{ action, result }` per harness-core; bind `result` and ignore `action` with `..`.

- [ ] **Step 5: Run the whole crate suite + workspace build**

Run: `cargo test -p harness-rs-mcp-client`
Run: `cargo build`
Expected: all pass; workspace builds with the new member.

- [ ] **Step 6: Commit**

```bash
git add crates/harness-mcp-client/tests
git commit -m "test(mcp-client): connect + in-loop integration (result flows through AgentLoop)"
```

---

## Task 6: README + wiring example

**Files:**
- Create: `crates/harness-mcp-client/README.md`

- [ ] **Step 1: Write the README**

```markdown
# harness-rs-mcp-client

Connect a harness `AgentLoop` to any MCP server over stdio. The server's tools
become harness `Arc<dyn Tool>`, so MCP results flow back through the standard
loop path (PreToolUse/PostToolUse, session record, context).

```rust
use harness_mcp_client::McpClient;

let mcp = McpClient::connect_stdio("cortexdb", &["mcp"]).await?;
let mut loop_ = harness_loop::AgentLoop::new(model);
for t in mcp.tools_with_read_only(&["graphrag_search"]) {
    loop_ = loop_.with_tool(t);
}
// keep `mcp` alive for the duration of the run.
```

Complements `harness-rs-mcp` (the server side). Transport: stdio. Capability:
tools only. See `docs/specs/2026-05-31-harness-mcp-client.md`.
```

- [ ] **Step 2: Commit**

```bash
git add crates/harness-mcp-client/README.md
git commit -m "docs(mcp-client): README + wiring example"
```

---

## Self-Review

- **Spec coverage:** generic stdio MCP client → Task 3; remote-tool→harness-Tool proxy + result mapping → Task 2; result re-enters the loop (the user's requirement) → Task 5 in-loop test asserting PostToolUse capture; read-only risk override → Task 3 `tools_with_read_only`; deterministic test via harness-mcp server → Tasks 4–5. Out of scope per spec: HTTP/SSE transports, auth, resources/prompts, KnowledgeGuide, liteparse.
- **Type consistency:** `build_schema(&McpTool)->ToolSchema` and `McpProxyTool::new(&McpTool, Peer, ToolRisk)` used identically in Tasks 2–3; `McpClient::{connect_stdio,tools,tools_with_read_only,tool_names}` consistent across Tasks 3, 5; `CallToolRequestParams::new(name)` + `.arguments` per verified rmcp API; `ToolResult { ok, content, trace }` mapping consistent.
- **Placeholder scan:** none — every step ships complete code or an exact command. Two explicitly-flagged fallbacks (McpTool struct-literal vs JSON-deserialize in Task 2; Event::PostToolUse field binding in Task 5) are compile-time forks with the resolution given inline.
- **Risk note:** proxied tools default to `Destructive` so a downstream permission gate (e.g. HAL's) treats remote calls conservatively; callers opt specific read-only tools out via `tools_with_read_only`.

---

## After this crate

- `cortexdb mcp` becomes the first real consumer: `McpClient::connect_stdio("cortexdb", &["mcp"])` → ingest/import/search tools in any agent.
- Then the liteparse front-door (PDF/office → text → CortexDB ingest) and the optional `KnowledgeGuide` (auto-inject graphrag_search results into `ctx.guides`).
- Later: HTTP/SSE transports behind features; surfacing server-reported usage into the loop's aggregate `Usage`.
