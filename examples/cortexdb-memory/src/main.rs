//! harness-rs agent using **CortexDB** as long-term memory over MCP.
//!
//! CortexDB ships a stdio MCP server (`cortexdb-mcp-stdio`). `harness-mcp-client`
//! spawns it, turns every CortexDB tool (knowledge_save, memory_search,
//! knowledge_graph_query, …) into a harness `Tool`, and any agent can then read
//! and write CortexDB's memory + knowledge graph.
//!
//! ```sh
//! # build CortexDB's MCP server once (from the CortexDB repo):
//! #   GOWORK=off go build -o /tmp/cortexdb-mcp-stdio ./cmd/cortexdb-mcp-stdio
//! export CORTEXDB_MCP_BIN=/tmp/cortexdb-mcp-stdio        # or have it on PATH
//!
//! # list the discovered tools (no API key needed):
//! cargo run -p cortexdb-memory
//!
//! # run the save→recall agent demo (lexical mode, no embedder key needed):
//! DASHSCOPE_API_KEY=sk-... cargo run -p cortexdb-memory
//! ```

use harness_context::default_world;
use harness_core::{Model, Task};
use harness_loop::AgentLoop;
use harness_mcp_client::McpClient;
use harness_models::ApiKind;
use std::sync::Arc;

const DASHSCOPE_BASE: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";
const MODEL: &str = "qwen3.7-plus";

/// CortexDB tools that only read — marked ReadOnly for risk classification.
const READ_ONLY: &[&str] = &[
    "knowledge_search",
    "memory_search",
    "memory_get",
    "knowledge_get",
    "cortex_query",
    "search_text",
    "build_context",
    "knowledge_memory_recall",
    "knowledge_graph_query",
];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Use a dedicated demo brain so we don't touch the global one.
    let db = std::env::temp_dir().join("cortexdb-harness-demo.db");
    unsafe { std::env::set_var("CORTEXDB_PATH", &db) };

    let bin = std::env::var("CORTEXDB_MCP_BIN").unwrap_or_else(|_| "cortexdb-mcp-stdio".into());
    println!("connecting to CortexDB MCP server: {bin}");
    println!("brain: {}\n", db.display());

    let cortex = McpClient::connect_stdio(&bin, &[])
        .await
        .map_err(|e| anyhow::anyhow!("spawn `{bin}` — build it or set CORTEXDB_MCP_BIN: {e}"))?;

    let names = cortex.tool_names();
    println!(
        "✅ connected — {} CortexDB MCP tools available:",
        names.len()
    );
    for chunk in names.chunks(4) {
        println!("   {}", chunk.join(", "));
    }

    let Ok(key) = std::env::var("DASHSCOPE_API_KEY") else {
        println!("\n(set DASHSCOPE_API_KEY to run the save → recall agent demo)");
        return Ok(());
    };

    let tools = cortex.tools_with_read_only(READ_ONLY);
    let model: Arc<dyn Model> = ApiKind::OpenAI.build(DASHSCOPE_BASE, MODEL, key);
    let mut agent = AgentLoop::new(harness_core::DynModel(model));
    for t in tools {
        agent = agent.with_tool(t);
    }
    let mut world = default_world(".");

    println!("\n=== agent: save a fact to CortexDB, then recall it ===\n");
    let task = Task {
        description: "You have CortexDB memory tools. First, save this fact to memory: \
                      \"Project Apollo ships on Friday and is owned by Alice.\" \
                      Then search memory for \"when does Apollo ship and who owns it?\" \
                      and tell me exactly what you retrieved."
            .into(),
        source: None,
        deadline: None,
    };
    let outcome = agent.run(task, &mut world).await?;
    println!("{outcome:?}");
    Ok(())
}
