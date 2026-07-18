//! Retail BI vertical (家电零售) — the agent asks DataIntelligence for governed
//! metrics, never raw SQL.
//!
//! The owner's question ("上周各门店各品类的销量和营收") is a *metric by
//! dimensions* — exactly what a semantic layer answers safely. The agent talks
//! to DI over MCP; DI compiles `units_sold`/`revenue` by `store_region` ×
//! `product_category` into fan-out-safe SQL, enforces RBAC (revenue is finance
//! only) and masks member phone. There is no `run_sql`, so the agent can't invent
//! a wrong join or fan a total out.
//!
//! The DI semantic model for this vertical lives in `di/model.yaml`; the warehouse
//! is `di/schema.sql`. Run (needs `di` + the seeded `applehub` warehouse):
//! ```sh
//! DI_BIN=/path/to/di \
//! DI_MODEL=$PWD/verticals/retail-bi-agent/di/model.yaml \
//! DI_DSN=postgres://user:pass@host:port/applehub?sslmode=disable \
//!   cargo run -p retail-bi-agent
//! ```

use harness_context::default_world;
use harness_core::{DynModel, Model, Task};
use harness_hooks::AuditHook;
use harness_loop::{AgentLoop, Outcome};
use harness_mcp_client::McpClient;
use harness_models::{ApiKind, MockModel, MockResponse};
use serde_json::json;
use std::sync::Arc;
use vertical_common::{PrintToolHook, open_audit, print_audit_and_verify, request_metadata};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// The model driving the agent: a real local model over Ollama's OpenAI-compatible
/// endpoint by default; set `LLM_MODEL=mock` for the deterministic scripted path.
fn build_model() -> Arc<dyn Model> {
    let name = env_or("LLM_MODEL", "qwen3.5:latest");
    if name == "mock" {
        return Arc::new(
            MockModel::new()
                .with_name("mock")
                .script(MockResponse::tool_call(
                    "query_metric",
                    json!({"metrics":["units_sold","revenue"],"group_by":["store_region","product_category"]}),
                ))
                .script(MockResponse::text("(mock) 已按治理口径取得各大区各品类销量与营收。")),
        );
    }
    let base = env_or("LLM_BASE", "http://localhost:11434/v1");
    ApiKind::OpenAI.build(base, name, "ollama")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let di_bin = env_or(
        "DI_BIN",
        "/Users/liliang/Things/AI/base/dataintelligence/di",
    );
    let model = env_or(
        "DI_MODEL",
        "/Users/liliang/Things/AI/base-rs/harness/verticals/retail-bi-agent/di/model.yaml",
    );
    let dsn = env_or(
        "DI_DSN",
        "postgres://reformd:reformd@localhost:47615/applehub?sslmode=disable",
    );

    println!("== 家电零售 BI 问数助手 (5 门店 · 站在 DataIntelligence 上) ==");
    println!("提问 (老板, 财务角色): 各门店大区各品类的销量和营收?\n");

    // The agent's data access is DI's governed semantic layer over MCP.
    let client = McpClient::connect_stdio(
        &di_bin,
        &["mcp", "-model", &model, "-dsn", &dsn, "-role", "finance"],
    )
    .await?;
    println!("[mcp] DI 治理工具: {:?} (无 run_sql)", client.tool_names());

    // Real local model decides which governed tool to call — no scripting.
    let model = build_model();
    println!("[llm] 驱动模型: {}\n", model.info().model);

    let (sink, audit_path) = open_audit("retail");
    let mut agent = AgentLoop::new(DynModel(model))
        .with_hook(Arc::new(AuditHook::new(sink)))
        .with_hook(Arc::new(PrintToolHook));
    for t in client.tools_with_read_only(&["list_metrics", "get_dimensions", "query_metric"]) {
        agent = agent.with_tool(t);
    }

    let metadata = request_metadata("boss@retail", "bi-1", "req-retail-1");
    let ws = std::env::temp_dir().join(format!("retail-ws-{}", std::process::id()));
    std::fs::create_dir_all(&ws)?;
    let mut world = default_world(&ws);
    let task = Task {
        description: "你是零售数据助手,只能通过治理工具查数、不得编造数字。\
             用 query_metric 查『各门店大区(store_region)× 各品类(product_category)』的\
             销量(units_sold)与营收(revenue),拿到结果后用中文简要汇总,并指出营收最高的品类。\
             如不确定可用的指标或维度,先调用 list_metrics / get_dimensions。"
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

    println!(
        "\n要点:老板问『指标×维度』,DI 保证 join/grain/fan-out 结构性正确 + RBAC/脱敏。\
         \n这个 vertical 自带 DI 语义模型(di/model.yaml)——交付时改的就是这份 + schema。"
    );
    Ok(())
}
