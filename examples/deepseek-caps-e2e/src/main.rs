//! End-to-end smoke test for three harness-rs capabilities against a real
//! DeepSeek model (OpenAI-compatible endpoint):
//!
//!   A) Recall — cross-session memory via FileRecall + session_search tool
//!   B) Learning loop — post-session skill/memory review fork
//!   C) Scheduler — in-process cron job that runs a DeepSeek agent turn
//!
//! Set DEEPSEEK_API_KEY before running.  Compile-only: `cargo build -p deepseek-caps-e2e`.

use async_trait::async_trait;
use harness_context::{FileMemory, FileRecall, default_world};
use harness_core::{Model, RecallStore, Task, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use harness_loop::{AgentLoop, LearningConfig, Outcome};
use harness_models::{OpenAiCompat, providers};
use harness_scheduler::{FileJobStore, Job, Scheduler, StdoutChannel};
use harness_tools_memory::RememberThisTool;
use harness_tools_skills::SkillManageTool;
use serde_json::json;
use std::sync::Arc;

// ── model helper ────────────────────────────────────────────────────────────

fn model(key: &str) -> OpenAiCompat {
    OpenAiCompat::with_key(providers::DEEPSEEK, "deepseek-v4-flash", key)
}

// ── ClockTool — triggers nudge on the main agent turn ───────────────────────

struct ClockTool {
    schema: ToolSchema,
}

impl ClockTool {
    fn new() -> Self {
        Self {
            schema: ToolSchema {
                name: "clock".into(),
                description: "Returns the current wall-clock time in milliseconds since epoch."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {}
                }),
            },
        }
    }
}

#[async_trait]
impl harness_core::Tool for ClockTool {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(
        &self,
        _args: serde_json::Value,
        world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            ok: true,
            content: json!({ "now_ms": world.clock.now_ms() }),
            trace: None,
        })
    }
}

// ── outcome text helper ─────────────────────────────────────────────────────

fn outcome_text(o: &Outcome) -> Option<&str> {
    match o {
        Outcome::Done { text, .. } => text.as_deref(),
        Outcome::BudgetExhausted { last_text, .. } => last_text.as_deref(),
    }
}

// ── unique temp path helper ──────────────────────────────────────────────────

fn tmp_dir(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "harness-e2e-{}-{}-{}",
        std::process::id(),
        nanos,
        label
    ))
}

// ═══════════════════════════════════════════════════════════════════════════
// 1) RECALL E2E
// ═══════════════════════════════════════════════════════════════════════════

async fn run_recall(key: &str) -> anyhow::Result<bool> {
    println!("\n─── [1/3] Recall e2e ───────────────────────────────────────");

    let recall_dir = tmp_dir("recall");
    let store: Arc<dyn RecallStore> = Arc::new(FileRecall::open(&recall_dir)?);

    // ── Run A: tell the model a fact ────────────────────────────────────────
    let mut world_a = default_world(".");
    world_a
        .profile
        .extra
        .insert("recall_owner".into(), json!("u1"));
    world_a
        .profile
        .extra
        .insert("recall_session".into(), json!("s1"));

    let loop_a = AgentLoop::new(model(key)).with_recall(store.clone());
    let task_a = Task {
        description: "Remember this fact about me: my favorite database is PostgreSQL. Reply in one short sentence acknowledging it.".into(),
        source: None,
        deadline: None,
    };
    let outcome_a = loop_a.run(task_a, &mut world_a).await?;
    println!(
        "  Run A reply: {}",
        outcome_text(&outcome_a).unwrap_or("<none>")
    );

    // ── Run B: new session, ask for the remembered fact ─────────────────────
    let mut world_b = default_world(".");
    world_b
        .profile
        .extra
        .insert("recall_owner".into(), json!("u1"));
    world_b
        .profile
        .extra
        .insert("recall_session".into(), json!("s2"));

    let loop_b = AgentLoop::new(model(key)).with_recall(store.clone());
    let task_b = Task {
        description: "Call the session_search tool to find what database I said I prefer in a past session, then tell me the database name.".into(),
        source: None,
        deadline: None,
    };
    let outcome_b = loop_b.run(task_b, &mut world_b).await?;
    let reply = outcome_text(&outcome_b).unwrap_or("").to_string();
    println!("  Run B reply: {reply}");

    let pass = reply.to_lowercase().contains("postgres");
    println!("  Recall: {}", if pass { "✅" } else { "❌" });

    // cleanup
    let _ = std::fs::remove_dir_all(&recall_dir);
    Ok(pass)
}

// ═══════════════════════════════════════════════════════════════════════════
// 2) LEARNING-LOOP E2E
// ═══════════════════════════════════════════════════════════════════════════

async fn run_learning(key: &str) -> anyhow::Result<bool> {
    println!("\n─── [2/3] Learning-loop e2e ────────────────────────────────");

    let mem_path = tmp_dir("mem").with_extension("jsonl");
    let skills_dir = tmp_dir("skills");

    // ensure parent dir for mem_path
    if let Some(p) = mem_path.parent() {
        std::fs::create_dir_all(p)?;
    }

    let mem: Arc<dyn harness_core::Memory> = Arc::new(FileMemory::open(&mem_path)?);
    let review_model: Arc<dyn Model> = Arc::new(model(key));

    let cfg = LearningConfig::new(review_model)
        .with_tool(Arc::new(SkillManageTool::new(&skills_dir)))
        .with_tool(Arc::new(RememberThisTool::new(mem.clone())))
        .with_nudge_interval(1);

    let mut world = default_world(".");
    let task = Task {
        description: "Important preference: from now on always address me as 'Captain'. \
            To begin, call the clock tool and tell me the current time."
            .into(),
        source: None,
        deadline: None,
    };

    let loop_ = AgentLoop::new(model(key))
        .with_tool(Arc::new(ClockTool::new()))
        .with_learning_loop(cfg);

    let outcome = loop_.run(task, &mut world).await?;
    println!(
        "  Main-agent reply: {}",
        outcome_text(&outcome).unwrap_or("<none>")
    );

    // Give the background review subagent a moment to complete (it runs inline
    // in run_learning_review which is awaited before run() returns, so we just
    // check immediately).
    let mems = mem.recall("", 10).await?;
    let skills_count = if skills_dir.exists() {
        std::fs::read_dir(&skills_dir)
            .map(|rd| rd.count())
            .unwrap_or(0)
    } else {
        0
    };

    println!(
        "  Review wrote {} memories, {} skill entries",
        mems.len(),
        skills_count
    );
    for m in &mems {
        println!("    mem: {}", m.content);
    }

    let pass = !mems.is_empty() || skills_count > 0;
    println!("  Learning: {}", if pass { "✅" } else { "❌" });

    // cleanup
    let _ = std::fs::remove_file(&mem_path);
    let _ = std::fs::remove_dir_all(&skills_dir);
    Ok(pass)
}

// ═══════════════════════════════════════════════════════════════════════════
// 3) SCHEDULER E2E
// ═══════════════════════════════════════════════════════════════════════════

async fn run_scheduler(key: &str) -> anyhow::Result<bool> {
    println!("\n─── [3/3] Scheduler e2e ────────────────────────────────────");

    let jobs_path = tmp_dir("jobs").with_extension("json");
    if let Some(p) = jobs_path.parent() {
        std::fs::create_dir_all(p)?;
    }

    let jobs: Arc<dyn harness_scheduler::JobStore> = Arc::new(FileJobStore::open(&jobs_path)?);

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let job = Job::new(
        "quote",
        "every 1m",
        "Write a one-sentence motivational quote. Reply with ONLY the quote text.",
        "stdout",
        now_ms,
    )
    .with_next_run(Some(0));

    jobs.add(&job).await?;

    let m: Arc<dyn Model> = Arc::new(model(key));
    let sched = Scheduler::new(jobs.clone(), m).with_channel(Arc::new(StdoutChannel::new()));

    let fired = sched.tick_once().await;
    println!("  Jobs fired: {fired}");

    let pass = fired == 1;
    println!("  Scheduler: {}", if pass { "✅" } else { "❌" });

    // cleanup
    let _ = std::fs::remove_file(&jobs_path);
    Ok(pass)
}

// ═══════════════════════════════════════════════════════════════════════════
// main
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let key = std::env::var("DEEPSEEK_API_KEY")
        .expect("set DEEPSEEK_API_KEY to a valid DeepSeek key before running this binary");

    let recall_ok = run_recall(&key).await?;
    let learning_ok = run_learning(&key).await?;
    let scheduler_ok = run_scheduler(&key).await?;

    println!("\n══════════════════════════════════════════════════════");
    println!(
        "e2e: recall={} learning={} scheduler={}",
        if recall_ok { "✅" } else { "❌" },
        if learning_ok { "✅" } else { "❌" },
        if scheduler_ok { "✅" } else { "❌" },
    );

    let all_pass = recall_ok && learning_ok && scheduler_ok;
    std::process::exit(if all_pass { 0 } else { 1 });
}
