//! Auto-repair chain ops vertical (汽修连锁) — a local model asks DataIntelligence
//! for governed metrics. work-order grain vs labor-capacity grain, so
//! `labor_utilization` is chasm-safe; parts revenue/margin are finance-gated and
//! customer phone masked.
//!
//! `di/model.yaml` + `di/schema.sql` are this customer's semantic model + warehouse
//! (10 shops, 40k work orders, 90k parts lines). Run (needs `di` + the `autorepair`
//! warehouse + Ollama):
//! ```sh
//! DI_MODEL=$PWD/projects/datainside/autorepair-ops-agent/di/model.yaml \
//! DI_DSN=postgres://user:pass@host:port/autorepair?sslmode=disable \
//!   cargo run -p autorepair-ops-agent
//! ```

use bi_common::{PrintToolHook, local_model, open_audit, print_audit_and_verify, request_metadata};
use harness_context::default_world;
use harness_core::{DynModel, Task};
use harness_hooks::AuditHook;
use harness_loop::{AgentLoop, Outcome};
use harness_mcp_client::McpClient;
use std::sync::Arc;

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
        concat!(env!("CARGO_MANIFEST_DIR"), "/di/model.yaml"),
    );
    let dsn = env_or(
        "DI_DSN",
        "postgres://reformd:reformd@localhost:47615/autorepair?sslmode=disable",
    );

    println!("== 汽修连锁 经营助手 (10 门店 · 4 万工单 · 站在 DataIntelligence 上) ==");
    println!("提问 (老板, 财务角色): 各门店的工时利用率、返修率,以及各品类的配件毛利率?\n");

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

    let (sink, audit_path) = open_audit("autorepair");
    let mut agent = AgentLoop::new(DynModel(model))
        .with_hook(Arc::new(AuditHook::new(sink)))
        .with_hook(Arc::new(PrintToolHook));
    for t in client.tools_with_read_only(&["list_metrics", "get_dimensions", "query_metric"]) {
        agent = agent.with_tool(t);
    }

    let metadata = request_metadata("boss@autorepair", "bi-1", "req-auto-1");
    let ws = std::env::temp_dir().join(format!("auto-ws-{}", std::process::id()));
    std::fs::create_dir_all(&ws)?;
    let mut world = default_world(&ws);
    let task = Task {
        description: "你是汽修连锁经营助手,只能用治理工具查数、不得编造。分两次查询:\
             (1) 各门店(shop_name)的 labor_utilization、rework_rate、labor_revenue;\
             (2) 各配件品类(part_category)的 parts_revenue、parts_margin、parts_margin_rate。\
             用中文简要汇总,指出工时利用率最低、返修率最高的门店,以及毛利率最差的品类(需关注)。\
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
        "\n要点:配件毛利/营收受 finance 授权、车主手机脱敏;工时利用率跨 work_order/capacity grain,DI 编译 chasm-safe。"
    );
    Ok(())
}
