use crate::proxy::McpProxyTool;
use harness_core::{Tool, ToolRisk};
use rmcp::ServiceExt;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use std::sync::Arc;
use tokio::process::Command;

/// A live MCP client session over a spawned child stdio server. Owns the
/// `RunningService` so the child stays alive for as long as this — *or any tool
/// it produced* — is held. The service is `Arc`-shared into every
/// [`McpProxyTool`], so you can drop the `McpClient` after wiring its tools into
/// a long-lived agent/server and the session keeps working.
pub struct McpClient {
    service: Arc<RunningService<RoleClient, ()>>,
    tools: Vec<rmcp::model::Tool>,
}

impl McpClient {
    async fn from_service(service: RunningService<RoleClient, ()>) -> anyhow::Result<Self> {
        let tools = service.list_all_tools().await?;
        Ok(Self {
            service: Arc::new(service),
            tools,
        })
    }

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
        Self::from_service(service).await
    }

    /// Connect to an MCP server over Streamable HTTP (MCP 2025-03-26 spec) using a
    /// default reqwest client.
    ///
    /// Requires the `http` crate feature (on by default).
    ///
    /// # Security
    ///
    /// The default client **follows HTTP redirects** and re-resolves DNS at
    /// connect time, so validating `url` up front does **not** prevent SSRF when
    /// `url` is untrusted: a `302 Location: http://169.254.169.254/…` (cloud
    /// metadata) or DNS rebinding to an internal address slips straight past a
    /// pre-flight check. For untrusted URLs use
    /// [`connect_http_with_client`](Self::connect_http_with_client) with a
    /// hardened client instead.
    #[cfg(feature = "http")]
    pub async fn connect_http(url: &str) -> anyhow::Result<Self> {
        let transport = rmcp::transport::StreamableHttpClientTransport::from_uri(url);
        let service = ()
            .serve(transport)
            .await
            .map_err(|e| anyhow::anyhow!("mcp http connect to `{url}` failed: {e}"))?;
        Self::from_service(service).await
    }

    /// Connect over Streamable HTTP using a **caller-supplied** [`reqwest::Client`].
    ///
    /// This is the SSRF-safe entry point: the caller owns the HTTP policy. A
    /// security-sensitive host can validate the URL, resolve the host to an
    /// allow-listed IP, then pass a client built with
    /// `reqwest::redirect::Policy::none()` and `.resolve(host, addr)` pinning the
    /// host to that validated IP — closing both the redirect-bypass and
    /// DNS-rebinding holes while keeping the security policy on the caller's side.
    ///
    /// The matching `reqwest` is re-exported as [`crate::reqwest`] so the client
    /// type unifies with the one rmcp expects.
    ///
    /// Requires the `http` crate feature.
    #[cfg(feature = "http")]
    pub async fn connect_http_with_client(
        url: &str,
        client: reqwest::Client,
    ) -> anyhow::Result<Self> {
        use rmcp::transport::streamable_http_client::{
            StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
        };
        let transport = StreamableHttpClientTransport::with_client(
            client,
            StreamableHttpClientTransportConfig::with_uri(url),
        );
        let service = ()
            .serve(transport)
            .await
            .map_err(|e| anyhow::anyhow!("mcp http connect to `{url}` failed: {e}"))?;
        Self::from_service(service).await
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
        self.tools
            .iter()
            .map(|t| {
                let risk = if read_only.contains(&t.name.as_ref()) {
                    ToolRisk::ReadOnly
                } else {
                    ToolRisk::Destructive
                };
                // Each tool holds the Arc'd session, keeping the child alive.
                Arc::new(McpProxyTool::new(t, self.service.clone(), risk)) as Arc<dyn Tool>
            })
            .collect()
    }
}
