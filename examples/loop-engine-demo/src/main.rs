//! End-to-end demo of `harness-loop-engine` against a real model.
//!
//! Runs ONE round of an L1 (report-only) loop: a maker sub-agent inspects
//! this repository with read-only filesystem tools, a checker sub-agent
//! verifies the findings, the token budget is tallied across both, and the
//! gate (AlwaysEscalate, as L1 demands) routes the result to a report.
//!
//! ```sh
//! DASHSCOPE_API_KEY=sk-... cargo run -p loop-engine-demo
//! # optional: pick a pattern — triage (default) | cleanup | issues
//! DASHSCOPE_API_KEY=sk-... cargo run -p loop-engine-demo -- cleanup
//! ```
//!
//! Uses Alibaba DashScope's OpenAI-compatible endpoint with `qwen3.7-plus`.

use anyhow::Context as _;
use harness_core::Model;
use harness_loop_engine::{LoopEngine, patterns};
use harness_models::ApiKind;
use harness_tools_fs::{ListDir, ReadFile};
use std::sync::Arc;

const DASHSCOPE_BASE: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";
const MODEL: &str = "qwen3.7-plus";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber_init();

    let key = std::env::var("DASHSCOPE_API_KEY")
        .context("set DASHSCOPE_API_KEY to your DashScope key")?;

    let pattern = std::env::args().nth(1).unwrap_or_else(|| "triage".into());
    let spec = match pattern.as_str() {
        "cleanup" => patterns::post_merge_cleanup(),
        "issues" => patterns::issue_triage(),
        _ => patterns::daily_triage(),
    };

    println!("== loop-engine demo ==");
    println!("loop:    {}", spec.name);
    println!("level:   {}", spec.level.label());
    println!("intent:  {}", spec.intent);
    println!("cadence: {}", spec.cadence);
    println!("running one round against {MODEL} …\n");

    // One entry point: protocol family + base_url + model + key.
    let model: Arc<dyn Model> = ApiKind::OpenAI.build(DASHSCOPE_BASE, MODEL, key);

    // L1 is read-only by construction; give both sub-agents the read tools.
    let engine = LoopEngine::new(spec, model)
        .with_maker_tool(Arc::new(ListDir))
        .with_maker_tool(Arc::new(ReadFile))
        .with_checker_tool(Arc::new(ListDir))
        .with_checker_tool(Arc::new(ReadFile));

    let report = engine.run_once().await;

    println!("---------- ROUND REPORT ----------");
    print!("{}", report.render());
    println!("----------------------------------");
    println!("outcome:       {:?}", report.outcome);
    println!("should_deliver: {}", report.should_deliver());
    println!("total tokens:  {}", report.total_tokens());

    Ok(())
}

fn tracing_subscriber_init() {
    // Best-effort; the demo works without it.
    let _ = std::panic::catch_unwind(|| {});
}
