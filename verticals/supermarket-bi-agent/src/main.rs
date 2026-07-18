//! Supermarket BI vertical (较大超市) — a local model asks DataIntelligence for
//! governed metrics. sales grain vs inventory grain, so `sell_through` is
//! chasm-safe; `revenue`/`gross_margin` are finance-gated and member phone masked.
//!
//! `di/model.yaml` + `di/schema.sql` are this customer's semantic model + warehouse
//! (1500 SKUs, 200k sales). Run (needs `di` + the `supermart` warehouse + Ollama):
//! ```sh
//! DI_MODEL=$PWD/verticals/supermarket-bi-agent/di/model.yaml \
//! DI_DSN=postgres://user:pass@host:port/supermart?sslmode=disable \
//!   cargo run -p supermarket-bi-agent
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
        "/Users/liliang/Things/AI/base-rs/harness/verticals/supermarket-bi-agent/di/model.yaml",
    );
    let dsn = env_or(
        "DI_DSN",
        "postgres://reformd:reformd@localhost:47615/supermart?sslmode=disable",
    );

    println!("== 超市 经营助手 (1500 SKU · 20 万销售 · 站在 DataIntelligence 上) ==");
    println!("提问 (店长, 财务角色): 各部门的营收、毛利、毛利率,以及各品类的动销率?\n");

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

    let (sink, audit_path) = open_audit("supermarket");
    let mut agent = AgentLoop::new(DynModel(model))
        .with_hook(Arc::new(AuditHook::new(sink)))
        .with_hook(Arc::new(PrintToolHook));
    for t in client.tools_with_read_only(&["list_metrics", "get_dimensions", "query_metric"]) {
        agent = agent.with_tool(t);
    }

    let metadata = request_metadata("manager@supermart", "bi-1", "req-mart-1");
    let ws = std::env::temp_dir().join(format!("mart-ws-{}", std::process::id()));
    std::fs::create_dir_all(&ws)?;
    let mut world = default_world(&ws);
    let task = Task {
        description: "你是超市经营助手,只能用治理工具查数、不得编造。分两次查询:\
             (1) 各部门(dept_name)的 revenue、gross_margin、margin_rate;\
             (2) 各品类(category)的 units_sold、sell_through。\
             用中文简要汇总,指出毛利率最低的部门和动销最差的品类(需关注)。\
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
        "\n要点:毛利/营收受 finance 授权、会员手机脱敏;动销率跨 sales/inventory grain,DI 编译 chasm-safe。"
    );
    Ok(())
}
