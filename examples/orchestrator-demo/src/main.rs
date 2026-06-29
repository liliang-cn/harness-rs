//! End-to-end demo of `harness-orchestrator` against a real model.
//!
//! Runs a fan-out/fan-in DAG: three independent "research" Jobs execute
//! concurrently, then an aggregator Job depends on all three and synthesizes
//! their results. State is persisted to a `FileRunStore` so the run is
//! resumable.
//!
//! ```sh
//! DASHSCOPE_API_KEY=sk-... cargo run -p orchestrator-demo
//! ```
//!
//! Uses Alibaba DashScope's OpenAI-compatible endpoint with `qwen3.7-plus`.

use anyhow::Context as _;
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

    // Fan-out: three topics researched concurrently, then aggregated.
    let dag = Dag::from_jobs([
        Job::new(
            "notion",
            "In 2 sentences, what is Notion best at? Be concrete.",
        ),
        Job::new(
            "airtable",
            "In 2 sentences, what is Airtable best at? Be concrete.",
        ),
        Job::new("coda", "In 2 sentences, what is Coda best at? Be concrete."),
        Job::new(
            "compare",
            "Using the three upstream results, write a 3-bullet comparison \
             of Notion vs Airtable vs Coda for a small team. One bullet each.",
        )
        .with_deps(["notion", "airtable", "coda"]),
    ]);

    let model: Arc<dyn Model> = ApiKind::OpenAI.build(DASHSCOPE_BASE, MODEL, key);
    let runner = Arc::new(SubagentJobRunner::new(model, ".").with_max_iters(4));
    let store = Arc::new(FileRunStore::open(
        std::env::temp_dir().join("harness-orch-demo"),
    )?);

    let run = Run::new("demo-run", "compare Notion/Airtable/Coda", dag)
        .with_budget(RunBudget::max_total_tokens(1_000_000));

    println!("== orchestrator demo ==");
    println!("DAG: notion ┐");
    println!("     airtable ┼─► compare");
    println!("     coda    ┘");
    println!("running against {MODEL} (3 concurrent + 1 aggregate) …\n");

    let orch = Orchestrator::new(runner)
        .with_store(store)
        .with_max_concurrency(3);
    let report = orch.run(run).await;

    println!("---------- RUN REPORT ----------");
    print!("{}", report.render());
    println!("--------------------------------");
    println!("state:        {:?}", report.state);
    println!("succeeded:    {}/{}", report.succeeded(), report.jobs.len());
    println!("total tokens: {}", report.spent_tokens);
    if let Some((_, _, Some(text))) = report.jobs.iter().find(|(id, _, _)| id == "compare") {
        println!("\n=== comparison ===\n{}", text.trim());
    }
    Ok(())
}
