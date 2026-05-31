use crate::proxy::McpProxyTool;
use harness_core::{Tool, ToolRisk};
use rmcp::ServiceExt;
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
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
