//! E-Commerce BI vertical (综合电商平台经营分析) — agent talks to DataIntelligence
//! for governed metrics over Postgres.

use bi_common::{PrintToolHook, local_model, open_audit, print_audit_and_verify, request_metadata};
use harness_context::default_world;
use harness_core::{DynModel, Task};
use harness_hooks::AuditHook;
use harness_loop::{AgentLoop, Outcome};
use harness_mcp_client::McpClient;
use std::sync::Arc;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let di_bin = env_or(
        "DI_BIN",
        "/Users/liliang/Things/AI/base/dataintelligence/di",
    );
    let model_path = env_or(
        "DI_MODEL",
        concat!(env!("CARGO_MANIFEST_DIR"), "/di/model.yaml"),
    );
    let dsn = env_or(
        "DI_DSN",
        "postgres://reformd:reformd@localhost:47615/ecommerce?sslmode=disable",
    );

    println!("== 电商平台经营分析 BI 助手 (30,000 订单 · 站在 DataIntelligence 治理层上) ==");
    println!("提问 (运营总监): 各渠道各品类的营收、毛利率与动销率?\n");

    let client = McpClient::connect_stdio(
        &di_bin,
        &[
            "mcp",
            "-model",
            &model_path,
            "-dsn",
            &dsn,
            "-role",
            "finance",
        ],
    )
    .await?;
    println!("[mcp] DI 治理工具: {:?} (无 run_sql)", client.tool_names());

    let model = local_model();
    println!("[llm] 驱动模型: {}\n", model.info().model);

    let (sink, audit_path) = open_audit("ecommerce");
    let mut agent = AgentLoop::new(DynModel(model))
        .with_hook(Arc::new(AuditHook::new(sink)))
        .with_hook(Arc::new(PrintToolHook));
    for t in client.tools_with_read_only(&["list_metrics", "get_dimensions", "query_metric"]) {
        agent = agent.with_tool(t);
    }

    let metadata = request_metadata("ecom_admin", "bi-ecom-1", "req-ecom-1");
    let ws = std::env::temp_dir().join(format!("ecom-ws-{}", std::process::id()));
    std::fs::create_dir_all(&ws)?;
    let mut world = default_world(&ws);
    let task = Task {
        description: "你是电商数据分析助手，只能通过治理工具查数，不得编造数字。\
             请使用 query_metric 查询『渠道(channel) × 品类(category)』的\
             营收(revenue)、毛利率(margin_rate)以及动销率(sell_through)，\
             拿到结果后用中文生成简明表格，并找出营收最高的渠道与品类组合。"
            .into(),
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
    drop(client);
    Ok(())
}
