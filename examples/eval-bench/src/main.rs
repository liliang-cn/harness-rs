//! Run a fixed agentic task through harness-rs and print an eval-go Sample
//! (JSON array) to stdout, for cross-framework benchmarking with eval-go.
//!
//! ```sh
//! DASHSCOPE_KEY=sk-... cargo run -p eval-bench > harness.json
//! ```
//!
//! Optional TASK_SEED is a JSON object {relpath: content} of files to
//! pre-create in the workspace before the run (a small file "database").
use harness::prelude::*;
use harness_context::default_world;
use harness_core::{Event, Hook, HookOutcome, Task};
use harness_loop::{AgentLoop, Outcome};
use harness_models::OpenAiCompat;
use harness_tools_fs::{ListDir, ReadFile, WriteFile};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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

/// Dump every file left in the workspace so the eval-go judge can grade against
/// actual end state, not just the final answer.
fn final_workspace(ws: &std::path::Path) -> String {
    let mut out = String::new();
    fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut String) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                let rel = p.strip_prefix(root).unwrap_or(&p).display();
                let mut body = std::fs::read_to_string(&p).unwrap_or_default();
                if body.len() > 4000 {
                    body.truncate(4000);
                }
                out.push_str(&format!("=== {rel} ===\n{body}\n"));
            }
        }
    }
    walk(ws, ws, &mut out);
    out
}

/// Read the eval-go ExecTarget variable (EVAL_*) first, then the legacy TASK_*
/// one — so this runner works both when driven by eval-go's Bench/ExecTarget and
/// by the standalone Python drivers.
fn env2(eval_key: &str, task_key: &str) -> String {
    std::env::var(eval_key)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var(task_key).ok())
        .unwrap_or_default()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let key = std::env::var("DASHSCOPE_KEY").expect("DASHSCOPE_KEY required");
    let mut task_id = env2("EVAL_NAME", "TASK_ID");
    if task_id.is_empty() {
        task_id = "baseline".into();
    }
    let prompt = env2("EVAL_INPUT", "TASK_PROMPT");
    let rubric = env2("EVAL_RUBRIC", "TASK_RUBRIC");
    let exp: Vec<String> = env2("EVAL_EXPECTED_TOOLS", "TASK_EXP")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    let ws = std::env::temp_dir().join(format!("bench-harness-{}", std::process::id()));
    std::fs::create_dir_all(&ws)?;

    // Seed the workspace file "database".
    let seed = env2("EVAL_FILES", "TASK_SEED");
    if !seed.is_empty()
        && let Ok(files) = serde_json::from_str::<HashMap<String, String>>(&seed)
    {
        for (p, c) in files {
            let full = ws.join(&p);
            if let Some(parent) = full.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(full, c);
        }
    }

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
        .with_tool(Arc::new(ListDir))
        .with_hook(Arc::new(Capture {
            calls: calls.clone(),
        }))
        .run(
            Task {
                description: prompt.clone(),
                source: None,
                deadline: None,
            },
            &mut world,
        )
        .await?;

    let (output, iters, usage) = match &outcome {
        Outcome::Done {
            text, iters, usage, ..
        } => (text.clone().unwrap_or_default(), *iters, usage.clone()),
        Outcome::BudgetExhausted {
            last_text,
            iters,
            usage,
            ..
        } => (last_text.clone().unwrap_or_default(), *iters, usage.clone()),
    };

    let tool_calls = calls.lock().unwrap().clone();
    let traj: Vec<String> = tool_calls
        .iter()
        .map(|c| format!("call {}", c["name"].as_str().unwrap_or("?")))
        .collect();

    let sample = serde_json::json!([{
        "name": format!("{task_id} [harness-rs/rust]"),
        "input": prompt,
        "output": output,
        "context": [format!("FINAL WORKSPACE FILES:\n{}", final_workspace(&ws))],
        "expected_tools": exp,
        "tool_calls": tool_calls,
        "trajectory": traj,
        "rubric": rubric,
        "meta": {
            "framework": "harness-rs", "lang": "rust", "task": task_id,
            // Real measured cost for this run — so the benchmark reports tokens,
            // not just correctness.
            "iters": iters,
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "tool_calls": tool_calls.len(),
        }
    }]);

    eprintln!("ANSWER: {:.200}", output);
    eprintln!(
        "COST: {iters} iters, {} tool-calls, {} in / {} out tokens",
        tool_calls.len(),
        usage.input_tokens,
        usage.output_tokens
    );
    eprintln!("TOOLS({}): {:?}", tool_calls.len(), traj);
    println!("{}", serde_json::to_string_pretty(&sample)?);
    Ok(())
}
