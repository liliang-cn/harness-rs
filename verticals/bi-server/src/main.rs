//! `bi-server` — the deployable HTTP service that fronts a governed-BI vertical.
//!
//! It wires the whole on-prem stack behind one endpoint:
//!
//! ```text
//! browser / aigui  ──HTTP(SSE)──>  bi-server (harness-serve)
//!                                     │  ChatService: local model + DI tools + audit
//!                                     ▼
//!                                  di mcp (governed semantic layer) ──> Postgres
//!                                     ▲
//!                                  Ollama (local model)
//! ```
//!
//! `POST /chat` (unary JSON) and `POST /chat/stream` (SSE, `ChatChunk` frames —
//! what a streaming UI like aigui consumes). The agent's only data access is DI's
//! governed `query_metric` (no raw SQL); every request is audited (hash-chained).
//!
//! Config via env (defaults target the local retail warehouse):
//! ```sh
//! DI_MODEL=$PWD/verticals/retail-bi-agent/di/model.yaml \
//! DI_DSN=postgres://reformd:reformd@localhost:47615/applehub?sslmode=disable \
//! PORT=43117  cargo run -p bi-server
//! ```

use harness_hooks::HashChainSink;
use harness_mcp_client::McpClient;
use harness_serve::{ChatService, InMemorySessions, OpenAuth, http};
use std::sync::Arc;
use vertical_common::local_model;

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let di_bin = env_or(
        "DI_BIN",
        "/Users/liliang/Things/AI/base/dataintelligence/di",
    );
    let model_yaml = env_or(
        "DI_MODEL",
        "/Users/liliang/Things/AI/base-rs/harness/verticals/retail-bi-agent/di/model.yaml",
    );
    let dsn = env_or(
        "DI_DSN",
        "postgres://reformd:reformd@localhost:47615/applehub?sslmode=disable",
    );
    let port = env_or("PORT", "43117");
    let audit_path = env_or("AUDIT", "/tmp/bi-server-audit.jsonl");

    // Governed data access: the agent gets DI's read-only metric tools over MCP.
    let client = McpClient::connect_stdio(
        &di_bin,
        &[
            "mcp",
            "-model",
            &model_yaml,
            "-dsn",
            &dsn,
            "-role",
            "finance",
        ],
    )
    .await?;
    let tools = client.tools_with_read_only(&["list_metrics", "get_dimensions", "query_metric"]);
    println!("[mcp] DI governed tools: {:?}", client.tool_names());

    // One shared ChatService: local model + DI tools + hash-chained audit.
    let mut svc = ChatService::new(
        local_model(),
        Arc::new(OpenAuth::new("boss")), // dev auth; swap for StaticTokenAuth in prod
        Arc::new(InMemorySessions::new()),
        std::env::temp_dir().join("bi-server-ws"),
    )
    .with_audit(Arc::new(HashChainSink::new(&audit_path)?))
    .with_instruction(
        "你是企业经营数据助手。回答任何经营/业务数字问题时,必须调用 query_metric 等治理工具查数,\
         绝不能说\"无法访问数据\",也不得编造数字。不确定指标或维度名时先调用 list_metrics / \
         get_dimensions。用中文简洁作答。",
    );
    for t in tools {
        svc = svc.with_tool(t);
    }

    let app = http::router(Arc::new(svc));
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("[bi-server] up on http://{addr}");
    println!("  POST /chat          {{\"session_id\":\"s1\",\"message\":\"各品类的营收\"}}");
    println!("  POST /chat/stream   (SSE — ChatChunk frames for a streaming UI)");
    println!("  audit → {audit_path}");

    // The MCP session is kept alive by the tools themselves (Arc'd into the
    // ChatService), so dropping the client here is safe — no lifetime juggling.
    drop(client);
    axum::serve(listener, app).await?;
    Ok(())
}
