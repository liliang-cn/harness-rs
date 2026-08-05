use harness_mcp_client::McpClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bin = "/Users/liliang/.codex/plugins/cache/cortexdb/cortexdb/2.57.0/bin/cortexdb-mcp";
    println!("Connecting to CortexDB MCP...");
    let cortex_client = McpClient::connect_stdio(bin, &[]).await?;
    println!("Connected! Tools: {}", cortex_client.tool_names().len());
    Ok(())
}
