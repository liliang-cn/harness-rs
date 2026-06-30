# cortexdb-memory

A harness-rs agent that uses **[CortexDB](https://github.com/liliang-cn/cortexdb)**
as long-term memory + knowledge graph, over **MCP**.

CortexDB ships a stdio MCP server (`cortexdb-mcp-stdio`). `harness-mcp-client`
spawns it and turns every CortexDB tool — `memory_save`, `memory_search`,
`knowledge_save`, `knowledge_graph_query`, `knowledge_memory_recall`, … (48 in
total) — into a harness `Tool` that any agent (`AgentLoop`, orchestrator
`SubagentJobRunner`, or loop-engine maker/checker) can call.

## Run it

```sh
# 1. Build CortexDB's MCP server once (from the CortexDB repo):
GOWORK=off go build -o /tmp/cortexdb-mcp-stdio ./cmd/cortexdb-mcp-stdio
export CORTEXDB_MCP_BIN=/tmp/cortexdb-mcp-stdio          # or put it on PATH

# 2. List the discovered tools (no API key, no embedder needed — lexical mode):
cargo run -p cortexdb-memory

# 3. Run the save → recall agent demo:
DASHSCOPE_API_KEY=sk-... cargo run -p cortexdb-memory
```

The demo agent saves a fact (`"Project Apollo ships on Friday and is owned by
Alice."`) to CortexDB, then searches memory for it and reports what it
retrieved — proving a harness-rs agent reads and writes CortexDB through MCP.

## How it connects

```rust
use harness_mcp_client::McpClient;

let cortex = McpClient::connect_stdio("cortexdb-mcp-stdio", &[]).await?;
// every CortexDB tool becomes an Arc<dyn Tool>; mark the read-only ones:
let tools = cortex.tools_with_read_only(&["memory_search", "knowledge_search", "cortex_query"]);
for t in tools { agent = agent.with_tool(t); }
```

`CORTEXDB_PATH` selects the SQLite brain file (this example uses a temp one so
it doesn't touch your global memory). CortexDB runs in no-embedder **lexical
mode** by default — no API key required for the memory layer itself.
