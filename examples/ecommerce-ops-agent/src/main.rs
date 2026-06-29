//! Autonomous e-commerce operations agent (the complex one).
//!
//! A nightly run in three governed stages over a *live* PostgreSQL database.
//!
//! **1. ANALYZE** — the orchestrator runs sales/inventory/reviews concurrently;
//! a dynamic-replanning Planner queries the DB for anomalies and spawns only the
//! deep-dives the data warrants (one of which retries through a simulated
//! transient failure); a synthesis job emits a structured action list.
//!
//! **2. GOVERN** — each action is classified by blast radius and run through a
//! maturity-level gate: reorders & small markdowns auto-apply (real DB writes
//! via an ActionExecutor); big markdowns and product pauses escalate to a human.
//!
//! **3. REMEMBER** — applied actions are written to memory; the next run recalls
//! them so it doesn't repeat itself.
//!
//! ```sh
//! ../ecommerce-analyst/setup.sh                              # postgres in docker
//! export DATABASE_URL=postgres://postgres:ecom@localhost:38520/shop
//! DASHSCOPE_API_KEY=sk-... cargo run -p ecommerce-ops-agent
//! DASHSCOPE_API_KEY=sk-... cargo run -p ecommerce-ops-agent -- --resume
//! ```

use ecommerce_analyst::db;
use ecommerce_analyst::sqltool::SqlQueryTool;
use ecommerce_ops_agent::{
    action, govern, memory, planner::AnomalyPlanner, runner::OpsJobRunner, schema,
    tools::MarketSignalTool,
};
use harness_context::FileMemory;
use harness_core::{Memory, Model, Tool};
use harness_models::ApiKind;
use harness_orchestrator::{
    Dag, FileRunStore, Job, Orchestrator, Run, RunBudget, SubagentJobRunner,
};
use std::sync::Arc;

const DASHSCOPE_BASE: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";
const MODEL: &str = "qwen3.7-plus";
const RUN_ID: &str = "ops-run";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let key =
        std::env::var("DASHSCOPE_API_KEY").map_err(|_| anyhow::anyhow!("set DASHSCOPE_API_KEY"))?;
    let resume = std::env::args().any(|a| a == "--resume");

    // --- live database: shop data (reused from ecommerce-analyst) + ops tables ---
    let pool = db::connect()
        .await
        .map_err(|e| anyhow::anyhow!("connect postgres (run ecommerce-analyst/setup.sh): {e}"))?;
    db::ensure_schema(&pool).await?;
    if !db::is_seeded(&pool).await? {
        println!("seeding shop data first …");
        db::seed(&pool).await?;
    }
    schema::ensure_ops_schema(&pool).await?;

    // --- memory (cross-run spine) ---
    let mem: Arc<dyn Memory> = Arc::new(FileMemory::open(
        std::env::temp_dir().join("harness-ops-memory.jsonl"),
    )?);
    let prior = memory::recall_prior(mem.as_ref()).await;

    // --- model + tools ---
    let model: Arc<dyn Model> = ApiKind::OpenAI.build(DASHSCOPE_BASE, MODEL, key);
    let sql: Arc<dyn Tool> = Arc::new(SqlQueryTool::new(pool.clone()));
    let market: Arc<dyn Tool> = Arc::new(MarketSignalTool::new());
    let runner = Arc::new(OpsJobRunner::new(
        SubagentJobRunner::new(model, ".")
            .with_tool(sql)
            .with_tool(market)
            .with_max_iters(12),
    ));

    // --- base DAG (deep-dives + synthesis are added dynamically by the planner) ---
    let dag = Dag::from_jobs([
        Job::new(
            "sales",
            "Sales analyst: using sql_query, summarize completed revenue, top/bottom products, \
             and margin by category. Keep it short and quantitative.",
        ),
        Job::new(
            "inventory",
            "Inventory analyst: using sql_query, summarize stock health — items at/below reorder \
             level and likely dead stock. Short and quantitative.",
        ),
        Job::new(
            "reviews",
            "CX analyst: using sql_query, summarize reputation — products with the worst average \
             ratings (>=3 reviews). Short and quantitative.",
        ),
    ]);

    let planner = Arc::new(AnomalyPlanner::new(pool.clone(), prior.clone()));
    let store = Arc::new(FileRunStore::open(
        std::env::temp_dir().join("harness-ops-runs"),
    )?);

    println!("== autonomous e-commerce ops agent ==");
    println!(
        "model: {MODEL}   mode: {}",
        if resume { "resume" } else { "fresh" }
    );
    if !prior.trim().is_empty() {
        println!(
            "recalled {} prior decision(s) from memory",
            prior.lines().count()
        );
    }
    println!("\n[stage 1] ANALYZE — concurrent DAG + dynamic replanning + retry …\n");

    let orch = Orchestrator::new(runner)
        .with_planner(planner)
        .with_store(store)
        .with_max_concurrency(3)
        .with_max_replans(2);

    let report = if resume {
        match orch.resume(RUN_ID).await {
            Some(r) => r,
            None => {
                println!("no saved run to resume — starting fresh");
                orch.run(new_run(dag)).await
            }
        }
    } else {
        orch.run(new_run(dag)).await
    };

    println!("---------- ANALYZE REPORT ----------");
    print!("{}", report.render());
    println!("------------------------------------");
    println!(
        "state: {:?}   jobs: {}   tokens: {}\n",
        report.state,
        report.jobs.len(),
        report.spent_tokens
    );

    // --- stage 2: parse proposed actions and govern them ---
    let actions = report
        .jobs
        .iter()
        .find(|(id, _, _)| id == "synthesize")
        .and_then(|(_, _, t)| t.clone())
        .map(|t| action::parse_actions(&t))
        .unwrap_or_default();

    println!(
        "[stage 2] GOVERN & ACT — {} proposed action(s) through the gate …",
        actions.len()
    );
    let summary = govern::govern_and_act(&pool, &actions).await?;
    println!("\n  ✅ auto-applied ({}):", summary.applied.len());
    for a in &summary.applied {
        println!("     {a}");
    }
    println!("  🔸 escalated to human ({}):", summary.escalated.len());
    for e in &summary.escalated {
        println!("     {e}");
    }

    // --- stage 3: remember ---
    memory::remember(mem.as_ref(), &summary.applied).await;
    println!(
        "\n[stage 3] REMEMBER — persisted {} applied action(s) to memory",
        summary.applied.len()
    );

    // --- live DB side-effect summary ---
    print_db_state(&pool).await;
    Ok(())
}

fn new_run(dag: Dag) -> Run {
    Run::new(RUN_ID, "run tonight's e-commerce operations", dag)
        .with_budget(RunBudget::max_total_tokens(6_000_000))
}

async fn count_rows(pool: &sqlx::PgPool, table: &str) -> i64 {
    use sqlx::Row;
    sqlx::query(&format!("SELECT COUNT(*) AS n FROM {table}"))
        .fetch_one(pool)
        .await
        .ok()
        .and_then(|r| r.try_get::<i64, _>("n").ok())
        .unwrap_or(0)
}

async fn print_db_state(pool: &sqlx::PgPool) {
    println!("\n--- live DB side effects (cumulative) ---");
    println!(
        "  purchase_orders: {}",
        count_rows(pool, "purchase_orders").await
    );
    println!(
        "  price_changes:   {}",
        count_rows(pool, "price_changes").await
    );
    println!(
        "  escalations:     {}",
        count_rows(pool, "escalations").await
    );
    println!(
        "  action_log:      {}",
        count_rows(pool, "action_log").await
    );
}
