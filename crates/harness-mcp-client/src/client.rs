use crate::proxy::McpProxyTool;
use harness_core::{Tool, ToolRisk};
use rmcp::ServiceExt;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

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
    ///
    /// A server that dies during the handshake fails with rmcp's `connection closed: initialize
    /// response`, which names no cause: `serve` has consumed the transport, so the child and its exit
    /// status are gone by the time the error is built and the server's own complaint — a database it
    /// could not open, a config it rejected — is nowhere in it. When that happens this asks the
    /// program itself what went wrong; see [`diagnose_stdio`].
    pub async fn connect_stdio(program: &str, args: &[&str]) -> anyhow::Result<Self> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let transport = TokioChildProcess::new(Command::new(program).configure(|cmd| {
            for a in &owned {
                cmd.arg(a);
            }
        }))?;
        let service = match ().serve(transport).await {
            Ok(service) => service,
            Err(e) => {
                let why = diagnose_stdio(program, &owned).await;
                return Err(anyhow::anyhow!("mcp init for `{program}` failed: {e}{why}"));
            }
        };
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

/// How long to wait for the re-run to answer, and then to exit.
const DIAGNOSE_ANSWER_TIMEOUT: Duration = Duration::from_secs(5);
const DIAGNOSE_EXIT_TIMEOUT: Duration = Duration::from_secs(2);
/// How much of the server's complaint to quote. Enough for a sentence with a path in it.
const STDERR_TAIL_BYTES: usize = 600;

/// Ask a stdio MCP server that just failed to come up what happened to it.
///
/// Runs the same program once more and drives the same handshake by hand, this time owning the child
/// so its fate is observable: the exit status and stderr of a server that died, the signal that killed
/// one that was killed, or the fact that it answered perfectly well — which says the binary is sound
/// and the failed start was transient, a different problem from a broken server and worth not
/// confusing with one.
///
/// Returns a sentence to append to the connect error, empty when nothing could be established. Costs
/// one extra spawn, on a path that has already failed.
async fn diagnose_stdio(program: &str, args: &[String]) -> String {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return format!(" (running `{program}` again to find out why also failed: {e})"),
    };

    // Held open until after the wait below: dropping the handle is EOF on the server's stdin, which a
    // well-behaved stdio server treats as "shut down" — closing it early would make every server look
    // like one that exits on its own.
    let mut stdin = child.stdin.take();
    if let Some(handle) = stdin.as_mut() {
        let _ = handle.write_all(HANDSHAKE.as_bytes()).await;
        let _ = handle.flush().await;
    }

    let answered = match child.stdout.take() {
        Some(stdout) => {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            matches!(
                timeout(DIAGNOSE_ANSWER_TIMEOUT, reader.read_line(&mut line)).await,
                Ok(Ok(read)) if read > 0
            )
        }
        None => false,
    };

    let status = match timeout(DIAGNOSE_EXIT_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        _ => {
            let _ = child.start_kill();
            None
        }
    };
    let complaint = match child.stderr.take() {
        Some(mut stderr) => {
            let mut text = String::new();
            let _ = timeout(DIAGNOSE_EXIT_TIMEOUT, stderr.read_to_string(&mut text)).await;
            stderr_tail(&text)
        }
        None => String::new(),
    };
    drop(stdin);

    match (answered, status) {
        // Sound binary, failed start: worth saying so plainly rather than letting it read as a broken
        // server, because the two call for opposite next moves.
        (true, _) => format!(
            " (`{program}` answers an initialize handshake when run again, so the binary itself works — this start failed transiently and retrying is reasonable)"
        ),
        (false, Some(status)) => match exit_cause(&status) {
            Some(cause) if !complaint.is_empty() => {
                format!(
                    " (running `{program}` again: it {cause} before answering, saying: {complaint})"
                )
            }
            Some(cause) => format!(
                " (running `{program}` again: it {cause} before answering, and wrote nothing to stderr)"
            ),
            None => String::new(),
        },
        (false, None) if !complaint.is_empty() => format!(
            " (running `{program}` again: it neither answered nor exited, saying: {complaint})"
        ),
        (false, None) => format!(
            " (running `{program}` again: it neither answered an initialize handshake nor exited within {}s, so it is hanging rather than crashing)",
            DIAGNOSE_ANSWER_TIMEOUT.as_secs()
        ),
    }
}

/// The initialize request `serve` would have sent, so the re-run reproduces the same handshake.
const HANDSHAKE: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","#,
    r#""capabilities":{},"clientInfo":{"name":"harness-diagnose","version":"1"}}}"#,
    "\n"
);

/// How the process ended, in words. `None` when it exited successfully — that says nothing about a
/// failed handshake and a sentence about it would only mislead.
fn exit_cause(status: &std::process::ExitStatus) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Some(format!("was killed by signal {signal}"));
        }
    }
    match status.code() {
        Some(0) | None => None,
        Some(code) => Some(format!("exited with status {code}")),
    }
}

/// The tail of what the server said, on one line and bounded.
///
/// The tail rather than the head: a server that logs its startup before failing puts the reason last.
fn stderr_tail(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let tail = match trimmed.char_indices().nth_back(STDERR_TAIL_BYTES) {
        Some((cut, _)) => format!("…{}", &trimmed[cut..]),
        None => trimmed.to_string(),
    };
    format!("{:?}", tail.replace('\n', " | "))
}
