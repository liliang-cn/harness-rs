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
//! DI_MODEL=$PWD/projects/datainside/retail-bi-agent/di/model.yaml \
//! DI_DSN=postgres://reformd:reformd@localhost:47615/applehub?sslmode=disable \
//! PORT=43117  cargo run -p bi-server
//! ```

use bi_common::local_model;
use harness_hooks::HashChainSink;
use harness_mcp_client::McpClient;
use harness_serve::{ChatService, CorsConfig, InMemorySessions, OpenAuth, http};
use std::sync::Arc;

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

/// The metric + dimension names declared in a DI model.yaml, so we can preload
/// the catalog into the system prompt and let the model skip the discovery
/// round-trips (`list_metrics` / `get_dimensions`) that dominate latency.
#[derive(serde::Deserialize)]
struct ModelCatalog {
    #[serde(default)]
    metrics: Vec<Named>,
    #[serde(default)]
    dimensions: Vec<Named>,
}
#[derive(serde::Deserialize)]
struct Named {
    name: String,
}

fn catalog_hint(model_yaml: &str) -> Option<String> {
    let text = std::fs::read_to_string(model_yaml).ok()?;
    let cat: ModelCatalog = serde_yaml::from_str(&text).ok()?;
    if cat.metrics.is_empty() {
        return None;
    }
    let metrics = cat
        .metrics
        .iter()
        .map(|m| m.name.as_str())
        .collect::<Vec<_>>()
        .join("、");
    let dims = cat
        .dimensions
        .iter()
        .map(|d| d.name.as_str())
        .collect::<Vec<_>>()
        .join("、");
    Some(format!(
        "\n本模型可用指标: {metrics}。\n可用维度: {dims}。\n直接调用 query_metric(metrics=[...], by=[...]) 查数,\
         指标/维度名就从上面两行里选,无需再调用 list_metrics 或 get_dimensions。\
         若 query_metric 报某组合不合法(跨口径/越权),照实说明并换合法维度,不要编造。"
    ))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let di_bin = env_or(
        "DI_BIN",
        "/Users/liliang/Things/AI/base/dataintelligence/di",
    );
    let model_yaml = env_or(
        "DI_MODEL",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../retail-bi-agent/di/model.yaml"
        ),
    );
    let dsn = env_or(
        "DI_DSN",
        "postgres://reformd:reformd@localhost:47615/applehub?sslmode=disable",
    );
    let port = env_or("PORT", "43117");
    let role = env_or("DI_ROLE", "finance");
    let audit_path = env_or("AUDIT", "/tmp/bi-server-audit.jsonl");

    // Governed data access: the agent gets DI's read-only metric tools over MCP.
    let client = McpClient::connect_stdio(
        &di_bin,
        &["mcp", "-model", &model_yaml, "-dsn", &dsn, "-role", &role],
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
    .with_instruction(format!(
        "你是企业经营数据助手。回答任何经营/业务数字问题时,必须调用 query_metric 等治理工具查数,\
         绝不能说\"无法访问数据\",也不得编造数字。用中文简洁作答,先给 markdown 表格。\
         只要结果含一个分类维度 + 至少一个数值指标,就【必须】在表格后另起一行输出一个 ```chart 代码块,\
         块内是合法的 ECharts option JSON(单一指标用柱状图 bar、比率/趋势可用折线 line),\
         x 轴 category 用维度值、series.data 用指标值,只用刚查到的数据、不要编造,也不要在 chart 块里写注释文字。\
         示例:```chart \
         {{\"xAxis\":{{\"type\":\"category\",\"data\":[\"A\",\"B\"]}},\
         \"yAxis\":{{\"type\":\"value\"}},\"series\":[{{\"type\":\"bar\",\"data\":[1,2]}}]}}```。{}",
        catalog_hint(&model_yaml).unwrap_or_default()
    ));
    for t in tools {
        svc = svc.with_tool(t);
    }

    // Permissive CORS so a browser/aigui page served from another origin (a
    // static dev server) can POST /chat/stream and read the SSE frames.
    let app = http::router_with_cors(Arc::new(svc), &CorsConfig::permissive());
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
