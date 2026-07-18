//! Gym vertical — the **proactive** blueprint, now standing on DataIntelligence.
//!
//! The daily churn/retention job no longer writes raw SQL. It asks DI's governed
//! fitness model (over MCP) for per-studio `fill_rate` / `no_show_rate` /
//! `active_members` — numbers that are fan-out/chasm-safe by construction — finds
//! the low-utilization **hotspot studios**, and dispatches a follow-up task to
//! each studio's manager via `harness-tools-tasks`. A `harness-rs-daemon` entry
//! fires this "daily 08:00"; here it runs once.
//!
//! Uses DI's existing `examples/fitness/model.yaml` against the seeded `reformd`
//! warehouse. Run (needs `di` + that warehouse):
//! ```sh
//! DI_BIN=/path/to/di DI_MODEL=/path/examples/fitness/model.yaml \
//! DI_DSN=postgres://user:pass@host:port/reformd?sslmode=disable \
//!   cargo run -p gym-membership-agent
//! ```

use harness_context::default_world;
use harness_core::Task;
use harness_hooks::AuditHook;
use harness_loop::{AgentLoop, Outcome};
use harness_mcp_client::McpClient;
use harness_models::{MockModel, MockResponse};
use harness_tools_tasks::{JsonFileStore, TaskFilter, TaskStore, make_tools};
use serde_json::json;
use std::sync::Arc;
use vertical_common::{PrintToolHook, open_audit, print_audit_and_verify, request_metadata};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn follow_up(name: &str, studio: &str, reason: &str) -> MockResponse {
    MockResponse::tool_call(
        "tasks_create",
        json!({
            "name": name,
            "kind": "one_off",
            "argv": ["notify-manager", "--studio", studio, "--reason", reason]
        }),
    )
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

    println!("== 健身房 留存预警 (每日 08:00 定时任务, 本次手动触发) ==");
    println!("[job] 用 DI 治理指标找低利用率场馆,派发跟进 —— 不写原始 SQL\n");

    // Governed metrics via DI over MCP; task dispatch is local.
    let client = McpClient::connect_stdio(
        &di_bin,
        &["mcp", "-model", &model, "-dsn", &dsn, "-role", "analyst"],
    )
    .await?;
    println!("[mcp] DI 治理工具: {:?}\n", client.tool_names());

    let tasks_path = std::env::temp_dir().join(format!("gym-tasks-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&tasks_path);
    let store: Arc<dyn TaskStore> = Arc::new(JsonFileStore::new(&tasks_path));

    // Ask DI for per-studio retention signals, then dispatch to the two lowest
    // fill_rate studios (Santa Monica 0.47, Flatiron 0.47 in the seeded data).
    let mock = MockModel::new()
        .with_name("local-qwen")
        .script(MockResponse::tool_call(
            "query_metric",
            json!({
                "metrics": ["fill_rate", "no_show_rate", "active_members"],
                "group_by": ["studio_name"]
            }),
        ))
        .script(follow_up(
            "跟进-Santa Monica-低利用率",
            "Reformd Santa Monica",
            "fill_rate 0.47 全网最低,排课/获客需复盘",
        ))
        .script(follow_up(
            "跟进-Flatiron-低利用率",
            "Reformd Flatiron",
            "fill_rate 0.47 偏低,评估黄金时段供给",
        ))
        .script(MockResponse::text(
            "按 DI 治理指标,Santa Monica 与 Flatiron 两馆 fill_rate 全网最低(~0.47),\
             已向其店长派发复盘跟进任务。爽约率各馆稳定在 0.15 左右。所有数值经语义层 \
             chasm-safe 编译,capacity 不会被预订数放大。",
        ));

    let (sink, audit_path) = open_audit("gym");
    let mut agent = AgentLoop::new(mock)
        .with_hook(Arc::new(AuditHook::new(sink)))
        .with_hook(Arc::new(PrintToolHook));
    for t in client.tools_with_read_only(&["list_metrics", "get_dimensions", "query_metric"]) {
        agent = agent.with_tool(t);
    }
    for t in make_tools(store.clone()) {
        agent = agent.with_tool(t);
    }

    let metadata = request_metadata("system@daily-churn-job", "churn-1", "req-churn-1");
    let ws = std::env::temp_dir().join(format!("gym-ws-{}", std::process::id()));
    std::fs::create_dir_all(&ws)?;
    let mut world = default_world(&ws);
    let task = Task {
        description: "查 DI 治理指标找低利用率场馆,给店长派发复盘跟进任务。".into(),
        source: None,
        deadline: None,
    };

    let outcome = agent
        .run_with_seed_and_metadata(task, Vec::new(), metadata, &mut world, 8)
        .await
        .map_err(|e| anyhow::anyhow!("run failed: {e}"))?;
    if let Outcome::Done { text, .. } = &outcome {
        println!("\n[job summary]\n{}\n", text.clone().unwrap_or_default());
    }

    println!("== 已派发的跟进任务队列 ==");
    for t in store.list(&TaskFilter::default()).await.unwrap() {
        println!("  · {} [{:?}] argv={:?}", t.name, t.status, t.argv);
    }
    println!();

    print_audit_and_verify(&audit_path);
    let _ = std::fs::remove_file(&tasks_path);
    let _ = std::fs::remove_dir_all(&ws);
    drop(client);

    println!(
        "\n要点:主动(定时)跑;指标走 DI 语义层(数不会因 fan-out 出错)、按场馆派发\
         跟进任务、全程审计防篡改。这是『被动问答』之外的『主动治理』形态。"
    );
    Ok(())
}
