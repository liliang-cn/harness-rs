//! Teahouse (茶馆) BI vertical — a local model asks DataIntelligence for governed
//! metrics. Orders (per sitting) and order_items (per tea) are different grains,
//! so `revenue` groups by room/staff while `tea_revenue` groups by 茶类 — DI keeps
//! them chasm-safe and refuses to group an order metric through the tea join.
//!
//! `di/model.yaml` + `di/schema.sql` are this customer's semantic model + warehouse.
//! Run (needs `di` + the `teahouse` warehouse + Ollama):
//! ```sh
//! DI_MODEL=$PWD/verticals/teahouse-bi-agent/di/model.yaml \
//! DI_DSN=postgres://user:pass@host:port/teahouse?sslmode=disable \
//!   cargo run -p teahouse-bi-agent
//! ```

use harness_context::default_world;
use harness_core::{DynModel, Task};
use harness_hooks::AuditHook;
use harness_loop::{AgentLoop, Outcome};
use harness_mcp_client::McpClient;
use std::sync::Arc;
use vertical_common::{
    PrintToolHook, local_model, open_audit, print_audit_and_verify, request_metadata,
};

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
        "/Users/liliang/Things/AI/base-rs/harness/verticals/teahouse-bi-agent/di/model.yaml",
    );
    let dsn = env_or(
        "DI_DSN",
        "postgres://reformd:reformd@localhost:47615/teahouse?sslmode=disable",
    );

    println!("== 茶馆 经营助手 (站在 DataIntelligence 上) ==");
    println!("提问 (店主, 财务角色): 各茶类的营收,以及各包间类型的客单价和连带率?\n");

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
    println!("[mcp] DI 治理工具: {:?} (无 run_sql)", client.tool_names());
    let model = local_model();
    println!("[llm] 本地驱动模型: {}\n", model.info().model);

    let (sink, audit_path) = open_audit("teahouse");
    let mut agent = AgentLoop::new(DynModel(model))
        .with_hook(Arc::new(AuditHook::new(sink)))
        .with_hook(Arc::new(PrintToolHook));
    for t in client.tools_with_read_only(&["list_metrics", "get_dimensions", "query_metric"]) {
        agent = agent.with_tool(t);
    }

    let metadata = request_metadata("owner@teahouse", "bi-1", "req-tea-1");
    let ws = std::env::temp_dir().join(format!("tea-ws-{}", std::process::id()));
    std::fs::create_dir_all(&ws)?;
    let mut world = default_world(&ws);
    let task = Task {
        description: "你是茶馆经营助手,只能用治理工具查数、不得编造。分两次查询:\
             (1) 各茶类(tea_category)的 tea_revenue、tea_units;\
             (2) 各包间类型(room_type)的 revenue、avg_ticket、items_per_order。\
             用中文简要汇总,指出最赚钱的茶类和最高客单价的包间类型。\
             如不确定指标/维度名,先调用 list_metrics / get_dimensions。"
            .into(),
        source: None,
        deadline: None,
    };

    let outcome = agent
        .run_with_seed_and_metadata(task, Vec::new(), metadata, &mut world, 8)
        .await
        .map_err(|e| anyhow::anyhow!("run failed: {e}"))?;
    if let Outcome::Done { text, .. } = &outcome {
        println!("\n[answer]\n{}\n", text.clone().unwrap_or_default());
    }

    print_audit_and_verify(&audit_path);
    let _ = std::fs::remove_dir_all(&ws);
    drop(client);
    println!(
        "\n要点:茶品营收按茶类(item grain)、流水按包间(order grain),DI 分别在各自 grain 聚合,不混。"
    );
    Ok(())
}
