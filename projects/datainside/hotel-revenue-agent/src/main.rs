//! Hotel & B&B revenue vertical (酒店 / 连锁民宿) — a local model asks
//! DataIntelligence for governed metrics. stay grain vs capacity grain, so
//! `occupancy`/`revpar` are chasm metrics DI compiles per-grain; `revenue`/
//! `revpar`/`adr` are finance-gated and guest phone masked.
//!
//! `di/model.yaml` + `di/schema.sql` are this customer's semantic model + warehouse
//! (12 hotels, 600 rooms, 60k stays). Run (needs `di` + the `hotel` warehouse + Ollama):
//! ```sh
//! DI_MODEL=$PWD/projects/datainside/hotel-revenue-agent/di/model.yaml \
//! DI_DSN=postgres://user:pass@host:port/hotel?sslmode=disable \
//!   cargo run -p hotel-revenue-agent
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
        "postgres://reformd:reformd@localhost:47615/hotel?sslmode=disable",
    );

    println!(
        "== 酒店 / 连锁民宿 收益助手 (12 店 · 600 房 · 6 万入住 · 站在 DataIntelligence 上) =="
    );
    println!("提问 (店长, 财务角色): 各门店的入住率、RevPAR、平均房价,以及各房型的入住率?\n");

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

    let (sink, audit_path) = open_audit("hotel");
    let mut agent = AgentLoop::new(DynModel(model))
        .with_hook(Arc::new(AuditHook::new(sink)))
        .with_hook(Arc::new(PrintToolHook));
    for t in client.tools_with_read_only(&["list_metrics", "get_dimensions", "query_metric"]) {
        agent = agent.with_tool(t);
    }

    let metadata = request_metadata("gm@hotel", "bi-1", "req-hotel-1");
    let ws = std::env::temp_dir().join(format!("hotel-ws-{}", std::process::id()));
    std::fs::create_dir_all(&ws)?;
    let mut world = default_world(&ws);
    let task = Task {
        description: "你是酒店/民宿收益助手,只能用治理工具查数、不得编造。分两次查询:\
             (1) 各门店(hotel_name)的 occupancy、revpar、adr;\
             (2) 各房型(room_type)的 occupancy、room_nights_sold。\
             用中文简要汇总,指出入住率最低的门店和 RevPAR 最高的门店(需关注)。\
             入住率是跨 stay/capacity grain 的比率,交给 DI 治理编译,不要自己拼 SQL。\
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
        "\n要点:营收/RevPAR/房价受 finance 授权、客人手机脱敏;入住率跨 stay/capacity grain,DI 编译 chasm-safe。"
    );
    Ok(())
}
