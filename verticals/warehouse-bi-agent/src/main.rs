//! Warehouse BI vertical — the harness-rs agent standing on **DataIntelligence**.
//!
//! The right way to give an agent BI: not a `run_sql` tool (which lets it invent
//! joins and fan out a total into a confident wrong number), but a governed
//! semantic layer. DataIntelligence exposes `list_metrics` / `get_dimensions` /
//! `query_metric` over MCP — and deliberately no `run_sql`. harness-rs connects
//! to it with its MCP client, and the agent asks for a *metric by dimensions*.
//! DI compiles that into fan-out/chasm-safe SQL and enforces RBAC / RLS / masking.
//!
//! Run (needs the `di` binary + a seeded warehouse):
//! ```sh
//! DI_BIN=/path/to/di DI_MODEL=/path/fitness/model.yaml \
//! DI_DSN=postgres://user:pass@host:port/db?sslmode=disable \
//!   cargo run -p warehouse-bi-agent
//! ```

use harness_context::default_world;
use harness_core::Task;
use harness_hooks::AuditHook;
use harness_loop::{AgentLoop, Outcome};
use harness_mcp_client::McpClient;
use harness_models::{MockModel, MockResponse};
use serde_json::json;
use std::sync::Arc;
use vertical_common::{PrintToolHook, open_audit, print_audit_and_verify, request_metadata};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let di_bin = env_or(
        "DI_BIN",
        "/Users/liliang/Things/AI/base/dataintelligence/di",
    );
    let model = env_or(
        "DI_MODEL",
        "/Users/liliang/Things/AI/base/dataintelligence/examples/fitness/model.yaml",
    );
    let dsn = env_or(
        "DI_DSN",
        "postgres://reformd:reformd@localhost:47615/reformd?sslmode=disable",
    );

    println!("== 仓库 BI vertical — harness-rs agent 站在 DataIntelligence 上 ==");
    println!("[mcp] 连 di mcp(受治理语义层),不给 agent 原始 SQL\n");

    // Connect the agent to DI's governed MCP gateway (spawns `di mcp` on stdio).
    let client = McpClient::connect_stdio(
        &di_bin,
        &["mcp", "-model", &model, "-dsn", &dsn, "-role", "finance"],
    )
    .await?;

    println!("[mcp] DI 暴露的受治理工具: {:?}", client.tool_names());
    println!("      没有 run_sql —— agent 结构上写不出原始 SQL\n");

    // A real local model would decide to call query_metric; scripted here so the
    // run is deterministic. The point is the *shape*: metric × dimension, no SQL.
    let mock = MockModel::new()
        .with_name("local-qwen")
        .script(MockResponse::tool_call(
            "query_metric",
            json!({
                "metrics": ["booked_count", "capacity_offered", "fill_rate", "no_show_rate"],
                "group_by": ["studio_region"]
            }),
        ))
        .script(MockResponse::text(
            "各大区场馆利用率(fill_rate)与爽约率(no_show_rate)已按治理口径取得。\
             数值由 DataIntelligence 语义层编译为 chasm-safe SQL 得出——capacity 不会被\
             每场的预订数放大,fill_rate 是真实利用率而非 fan-out 膨胀后的错数。",
        ));

    let (sink, audit_path) = open_audit("warehouse-bi");
    let mut agent = AgentLoop::new(mock)
        .with_hook(Arc::new(AuditHook::new(sink)))
        .with_hook(Arc::new(PrintToolHook));
    for t in client.tools_with_read_only(&[
        "list_metrics",
        "get_dimensions",
        "query_metric",
        "describe_warehouse",
        "health_check",
    ]) {
        agent = agent.with_tool(t);
    }

    let metadata = request_metadata("finance@reformd", "bi-1", "req-wh-1");
    let ws = std::env::temp_dir().join(format!("wh-ws-{}", std::process::id()));
    std::fs::create_dir_all(&ws)?;
    let mut world = default_world(&ws);
    let task = Task {
        description: "按大区看场馆利用率(fill_rate)和爽约率(no_show_rate)".into(),
        source: None,
        deadline: None,
    };

    let outcome = agent
        .run_with_seed_and_metadata(task, Vec::new(), metadata, &mut world, 6)
        .await
        .map_err(|e| anyhow::anyhow!("run failed: {e}"))?;
    if let Outcome::Done { text, .. } = &outcome {
        println!("\n[answer]\n{}\n", text.clone().unwrap_or_default());
    }

    print_audit_and_verify(&audit_path);
    let _ = std::fs::remove_dir_all(&ws);
    drop(client); // its child `di mcp` backs the tools — keep alive until here

    println!(
        "\n要点:agent 只问『指标×维度』,DI 编译 chasm-safe SQL 并施加 RBAC/RLS/masking。\
         \n对照:让 agent 写原始 SQL,fan-out 会把 capacity 乘上每场预订数,fill_rate 从 \
         0.68 错成 0.074——跑得干净、数是错的。这就是 harness-tools-sql 只能当回退、\
         真 BI 必须走 DI 的原因。"
    );
    Ok(())
}
