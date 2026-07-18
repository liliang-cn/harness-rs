//! Forging-company ops vertical (2 工厂 / 15 产线) — a local model asks
//! DataIntelligence for governed quality/output metrics, never raw SQL.
//!
//! Quality metrics are exactly where naive text-to-SQL kills you: defect_rate
//! joins the defects table to the inspections table at different grains, and a
//! raw join fans the sampled count out per defect — reporting ~2.5% when the true
//! rate is ~10%. DI compiles it chasm-safe. The `di/model.yaml` here is the
//! semantic model for this customer; `di/schema.sql` is the warehouse.
//!
//! Run (needs `di` + the seeded `forge` warehouse + Ollama):
//! ```sh
//! DI_MODEL=$PWD/verticals/forge-ops-agent/di/model.yaml \
//! DI_DSN=postgres://user:pass@host:port/forge?sslmode=disable \
//!   cargo run -p forge-ops-agent
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

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let di_bin = env_or(
        "DI_BIN",
        "/Users/liliang/Things/AI/base/dataintelligence/di",
    );
    let model_yaml = env_or(
        "DI_MODEL",
        "/Users/liliang/Things/AI/base-rs/harness/verticals/forge-ops-agent/di/model.yaml",
    );
    let dsn = env_or(
        "DI_DSN",
        "postgres://reformd:reformd@localhost:47615/forge?sslmode=disable",
    );

    println!("== 锻造公司 经营助手 (2 厂 · 15 产线 · 生产/质量/销售/研发/排产) ==");
    println!(
        "提问 (厂长, 财务角色): 两个厂的经营概览 —— 产量、良率、缺陷率、产能利用率;\
             再看各客户等级的销售额与准时交付率。\n"
    );

    // finance role: can see revenue / R&D spend (RBAC-gated metrics).
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

    let (sink, audit_path) = open_audit("forge");
    let mut agent = AgentLoop::new(DynModel(model))
        .with_hook(Arc::new(AuditHook::new(sink)))
        .with_hook(Arc::new(PrintToolHook));
    for t in client.tools_with_read_only(&["list_metrics", "get_dimensions", "query_metric"]) {
        agent = agent.with_tool(t);
    }

    let metadata = request_metadata("qc@forge", "ops-1", "req-forge-1");
    let ws = std::env::temp_dir().join(format!("forge-ws-{}", std::process::id()));
    std::fs::create_dir_all(&ws)?;
    let mut world = default_world(&ws);
    let task = Task {
        description: "你是锻造公司经营助手,只能通过治理工具查数、不得编造。分两次查询:\
             (1) 各工厂(factory_name)的 output_units、yield_rate、defect_rate、capacity_utilization;\
             (2) 各客户等级(customer_tier)的 sales_revenue、on_time_rate。\
             然后用中文给一段经营概览,点出最需要关注的问题。\
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
        "\n要点:质量指标(defect_rate)跨 grain,DI 编译 chasm-safe SQL——原始 SQL fan-out \
         会把缺陷率从真实 ~10% 错报成 ~2.5%,主机厂审核时这种错数是事故。"
    );
    Ok(())
}
