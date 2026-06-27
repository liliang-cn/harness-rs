//! Run a fixed agentic task through harness-rs and print an eval-go Sample
//! (JSON array) to stdout, for cross-framework benchmarking with eval-go.
//!
//! ```sh
//! DASHSCOPE_KEY=sk-... cargo run -p eval-bench > harness.json
//! ```
use harness::prelude::*;
use harness_context::default_world;
use harness_core::{Event, Hook, HookOutcome, Task};
use harness_loop::{AgentLoop, Outcome};
use harness_models::OpenAiCompat;
use harness_tools_fs::{ReadFile, WriteFile};
use std::sync::{Arc, Mutex};

const TASK: &str = "Create a file named plan.txt containing exactly three TODO items, one per line. \
Then read the file back and tell me how many TODO items it contains.";

/// Records every tool call (name + args) the loop makes, for the eval-go Sample.
struct Capture {
    calls: Arc<Mutex<Vec<serde_json::Value>>>,
}
impl Hook for Capture {
    fn name(&self) -> &str {
        "capture"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PreToolUse { .. })
    }
    fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        if let Event::PreToolUse { action } = ev {
            self.calls.lock().unwrap().push(serde_json::json!({
                "name": action.tool,
                "args": action.args,
            }));
        }
        HookOutcome::Allow
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let key = std::env::var("DASHSCOPE_KEY").expect("DASHSCOPE_KEY required");
    let ws = std::env::temp_dir().join(format!("bench-harness-{}", std::process::id()));
    std::fs::create_dir_all(&ws)?;

    let model = OpenAiCompat::with_key(
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
        "qwen3.7-plus",
        key,
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut world = default_world(&ws);

    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(WriteFile))
        .with_tool(Arc::new(ReadFile))
        .with_hook(Arc::new(Capture {
            calls: calls.clone(),
        }))
        .run(
            Task {
                description: TASK.into(),
                source: None,
                deadline: None,
            },
            &mut world,
        )
        .await?;

    let output = match &outcome {
        Outcome::Done { text, .. } => text.clone().unwrap_or_default(),
        Outcome::BudgetExhausted { last_text, .. } => last_text.clone().unwrap_or_default(),
    };

    let tool_calls = calls.lock().unwrap().clone();
    let traj: Vec<String> = tool_calls
        .iter()
        .map(|c| format!("call {}", c["name"].as_str().unwrap_or("?")))
        .collect();

    let sample = serde_json::json!([{
        "name": "plan-write-readback [harness-rs/rust]",
        "input": TASK,
        "output": output,
        "expected_tools": ["write_file", "read_file"],
        "tool_calls": tool_calls,
        "trajectory": traj,
        "rubric": "PASS only if a file with exactly three TODO items was created AND the final answer correctly states the count is 3.",
        "meta": {"framework": "harness-rs", "lang": "rust"}
    }]);

    eprintln!("ANSWER: {:.200}", output);
    eprintln!("TOOLS: {:?}", traj);
    println!("{}", serde_json::to_string_pretty(&sample)?);
    Ok(())
}
