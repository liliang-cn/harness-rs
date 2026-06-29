//! E-commerce analyst agent.
//!
//! Runs three analysis Jobs concurrently — **sales**, **inventory**,
//! **reviews** — each querying a *live PostgreSQL database* with real SQL via
//! the `sql_query` tool, then a **report** Job synthesizes them into an
//! executive summary with prioritized actions. Orchestrated by
//! `harness-orchestrator` (concurrent DAG + dependency fan-in + run budget +
//! resumable state).
//!
//! ```sh
//! ./examples/ecommerce-analyst/setup.sh                       # postgres in docker
//! export DATABASE_URL=postgres://postgres:ecom@localhost:38520/shop
//! cargo run -p ecommerce-analyst --bin seed                   # realistic data
//! DASHSCOPE_API_KEY=sk-... cargo run -p ecommerce-analyst     # run the analyst
//! ```

use anyhow::Context as _;
use ecommerce_analyst::db;
use ecommerce_analyst::sqltool::SqlQueryTool;
use harness_core::Model;
use harness_models::ApiKind;
use harness_orchestrator::{
    Dag, FileRunStore, Job, Orchestrator, Run, RunBudget, SubagentJobRunner,
};
use std::sync::Arc;

const DASHSCOPE_BASE: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";
const MODEL: &str = "qwen3.7-plus";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let key = std::env::var("DASHSCOPE_API_KEY")
        .context("set DASHSCOPE_API_KEY to your DashScope key")?;

    // --- live database ---
    let pool = db::connect()
        .await
        .context("connect to postgres — run examples/ecommerce-analyst/setup.sh first")?;
    db::ensure_schema(&pool).await?;
    if !db::is_seeded(&pool).await? {
        println!("database empty — seeding realistic data first …");
        let s = db::seed(&pool).await?;
        println!(
            "seeded {} products / {} orders / {} order_items / {} reviews\n",
            s.products, s.orders, s.order_items, s.reviews
        );
    }

    // --- analyst DAG: 3 concurrent analyses → 1 synthesis ---
    let dag = Dag::from_jobs([
        Job::new(
            "sales",
            "You are a sales analyst. Using the sql_query tool, investigate revenue \
             performance: total completed revenue, the top 5 and bottom 5 products by \
             revenue, gross margin by category, revenue by channel, and whether there \
             was any unusual spike in daily orders. Then write a concise findings \
             summary with concrete numbers (convert cents to dollars).",
        ),
        Job::new(
            "inventory",
            "You are an inventory analyst. Using the sql_query tool, find stock risks: \
             products at or below their reorder_level, likely dead stock (high stock_qty \
             but low recent units sold), and an estimate of days-of-cover for the \
             fastest movers. Write concise findings with specific SKUs and numbers.",
        ),
        Job::new(
            "reviews",
            "You are a customer-experience analyst. Using the sql_query tool, assess \
             reputation: average rating and review count per product, the products with \
             the worst average ratings (min 3 reviews), and overall rating distribution. \
             Identify reputation-risk SKUs and summarize the likely complaint themes.",
        ),
        Job::new(
            "report",
            "You are the head of e-commerce. Using the three upstream analyses (sales, \
             inventory, reviews), write a tight executive brief: 3-sentence state of the \
             business, then a prioritized action list (max 6 items) ordered by business \
             impact, each with the concrete data point that justifies it. Do not run new \
             queries — synthesize what the analysts found.",
        )
        .with_deps(["sales", "inventory", "reviews"]),
    ]);

    // --- wire model + SQL tool + orchestrator ---
    let model: Arc<dyn Model> = ApiKind::OpenAI.build(DASHSCOPE_BASE, MODEL, key);
    let runner = Arc::new(
        SubagentJobRunner::new(model, ".")
            .with_tool(Arc::new(SqlQueryTool::new(pool.clone())))
            .with_max_iters(12),
    );
    let store = Arc::new(FileRunStore::open(
        std::env::temp_dir().join("harness-ecom-run"),
    )?);

    println!("== e-commerce analyst ==");
    println!("DAG: sales ┐");
    println!("     inventory ┼─► report");
    println!("     reviews  ┘");
    println!("3 analysts query the live DB concurrently, then synthesize. Model: {MODEL}\n");

    let run = Run::new("ecom-analysis", "analyze the shop's performance", dag)
        .with_budget(RunBudget::max_total_tokens(3_000_000));
    let report = Orchestrator::new(runner)
        .with_store(store)
        .with_max_concurrency(3)
        .run(run)
        .await;

    println!("---------- RUN REPORT ----------");
    print!("{}", report.render());
    println!("--------------------------------");
    println!(
        "state: {:?}   succeeded: {}/{}   tokens: {}",
        report.state,
        report.succeeded(),
        report.jobs.len(),
        report.spent_tokens
    );
    if let Some((_, _, Some(text))) = report.jobs.iter().find(|(id, _, _)| id == "report") {
        println!("\n=== EXECUTIVE BRIEF ===\n{}", text.trim());
    }
    Ok(())
}
