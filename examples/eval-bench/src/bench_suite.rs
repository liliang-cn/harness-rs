//! Task-completion benchmark — the `pass@1` runner.
//!
//! Where `eval-bench` (the other bin) measures *cost* on one task, this measures
//! whether the agent actually *solved* the task. Each task carries a machine
//! verifier (a shell assertion that exits 0 when the workspace end-state is
//! correct); the harness runs the verifier itself, outside the agent, so
//! "resolved" is an objective fact, not the model grading its own homework.
//!
//! This is the Rust-native, self-built task set: small, deterministic, no
//! network, no Docker. It exists to make "can it work autonomously?" a number
//! we can regress on. SWE-bench-lite (Python, per-instance containers via
//! `ContainerSandbox`) is the next phase and reuses this same runner shape.
//!
//! ```sh
//! HARNESS_API_KEY=sk-... cargo run -p eval-bench --bin bench-suite
//! # or, matching the existing eval-bench:
//! DASHSCOPE_KEY=sk-... cargo run -p eval-bench --bin bench-suite
//! ```
use harness::prelude::*;
use harness_context::default_world;
use harness_core::{Event, Hook, HookOutcome, Task};
use harness_loop::{AgentLoop, Outcome};
use harness_models::OpenAiCompat;
use harness_tools_fs::{EditFile, Grep, ListDir, ReadFile, WriteFile};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One benchmark task: a prompt, the files it starts from, and a shell
/// assertion that decides — objectively — whether the end-state is correct.
struct BenchTask {
    id: &'static str,
    prompt: &'static str,
    /// (relative path, contents) written into a fresh workspace before the run.
    seed: &'static [(&'static str, &'static str)],
    /// `bash -c` snippet run in the workspace after the run. Exit 0 = resolved.
    verify: &'static str,
}

/// The self-built Rust-native task set. Every task exercises the fs tools
/// (read/write/edit/list/grep) and has a deterministic, network-free verifier.
const TASKS: &[BenchTask] = &[
    BenchTask {
        id: "sum-file",
        prompt: "Read the file nums.txt in the workspace. It contains one integer \
                 per line. Compute their sum and write ONLY the resulting number \
                 (no other text) to a new file named sum.txt.",
        seed: &[("nums.txt", "10\n15\n17\n")],
        verify: r#"test "$(tr -d '[:space:]' < sum.txt)" = "42""#,
    },
    BenchTask {
        id: "rename-key",
        prompt: "In config.json, rename the JSON key \"old_name\" to \"new_name\". \
                 Keep its value and every other key unchanged.",
        seed: &[(
            "config.json",
            "{\"old_name\": \"server-1\", \"port\": 3477}\n",
        )],
        verify: r#"grep -q '"new_name"' config.json && ! grep -q '"old_name"' config.json && grep -q 'server-1' config.json"#,
    },
    BenchTask {
        id: "count-lines",
        prompt: "Count how many lines are in data.txt and write ONLY that count \
                 (a single number) to a file named count.txt.",
        seed: &[("data.txt", "a\nb\nc\nd\ne\nf\ng\n")],
        verify: r#"test "$(tr -d '[:space:]' < count.txt)" = "7""#,
    },
    BenchTask {
        id: "fix-typo",
        prompt: "In notes.txt, replace every occurrence of the misspelling \"teh\" \
                 with the correct \"the\". Change nothing else.",
        seed: &[("notes.txt", "teh cat sat on teh mat\n")],
        verify: r#"! grep -q 'teh' notes.txt && grep -q 'the cat sat on the mat' notes.txt"#,
    },
    BenchTask {
        id: "create-readme",
        prompt: "Create a file named README.md whose contents include the exact \
                 word BENCHMARK in uppercase.",
        seed: &[],
        verify: r#"grep -q 'BENCHMARK' README.md"#,
    },
];

/// Per-run outcome we report on.
struct Row {
    id: &'static str,
    resolved: bool,
    status: &'static str, // "resolved" | "wrong" | "timeout" | "error"
    iters: u32,
    tool_calls: usize,
    in_tok: u32,
    out_tok: u32,
    ms: u128,
}

/// Records how many tool calls the loop made (for the cost column).
struct Capture {
    n: Arc<Mutex<usize>>,
}
impl Hook for Capture {
    fn name(&self) -> &str {
        "bench-capture"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PreToolUse { .. })
    }
    fn fire(&self, _ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        *self.n.lock().unwrap() += 1;
        HookOutcome::Allow
    }
}

/// Read HARNESS_* first, fall back to the eval-bench DASHSCOPE_* convention.
fn model_from_env() -> OpenAiCompat {
    let key = std::env::var("HARNESS_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("DASHSCOPE_KEY").ok())
        .expect("set HARNESS_API_KEY (or DASHSCOPE_KEY)");
    let base = std::env::var("HARNESS_BASE_URL")
        .unwrap_or_else(|_| "https://dashscope.aliyuncs.com/compatible-mode/v1".into());
    let model = std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "qwen3.7-plus".into());
    OpenAiCompat::with_key(base, model, key)
}

async fn run_task(task: &BenchTask) -> Row {
    // Fresh, isolated workspace per task.
    let ws = std::env::temp_dir().join(format!("bench-suite-{}-{}", std::process::id(), task.id));
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).expect("create workspace");
    for (rel, body) in task.seed {
        let full = ws.join(rel);
        if let Some(p) = full.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        std::fs::write(full, body).expect("seed file");
    }

    let n = Arc::new(Mutex::new(0usize));
    let mut world = default_world(&ws);
    let started = Instant::now();

    let agent = AgentLoop::new(model_from_env())
        .with_tool(Arc::new(ReadFile))
        .with_tool(Arc::new(WriteFile))
        .with_tool(Arc::new(EditFile))
        .with_tool(Arc::new(ListDir))
        .with_tool(Arc::new(Grep))
        .with_hook(Arc::new(Capture { n: n.clone() }));
    let fut = agent.run(
        Task {
            description: task.prompt.into(),
            source: None,
            deadline: None,
        },
        &mut world,
    );

    // Runaway loops count as failures, not hangs — this is a metric, not a crash.
    let result = tokio::time::timeout(Duration::from_secs(120), fut).await;
    let ms = started.elapsed().as_millis();
    let tool_calls = *n.lock().unwrap();

    let (status_run, iters, in_tok, out_tok) = match result {
        Ok(Ok(Outcome::Done { iters, usage, .. })) => {
            ("done", iters, usage.input_tokens, usage.output_tokens)
        }
        Ok(Ok(Outcome::BudgetExhausted { iters, usage, .. })) => {
            ("budget", iters, usage.input_tokens, usage.output_tokens)
        }
        Ok(Ok(Outcome::Stuck { iters, usage, .. })) => {
            ("stuck", iters, usage.input_tokens, usage.output_tokens)
        }
        Ok(Err(e)) => {
            eprintln!("  ! run error: {e}");
            ("error", 0, 0, 0)
        }
        Err(_) => ("timeout", 0, 0, 0),
    };

    // The verifier: an objective assertion we run ourselves, outside the agent.
    let verified = Command::new("bash")
        .arg("-c")
        .arg(task.verify)
        .current_dir(&ws)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let status = match (status_run, verified) {
        (_, true) => "resolved",
        ("timeout", false) => "timeout",
        ("error", false) => "error",
        ("stuck", false) => "stuck",
        (_, false) => "wrong",
    };

    Row {
        id: task.id,
        resolved: verified,
        status,
        iters,
        tool_calls,
        in_tok,
        out_tok,
        ms,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    eprintln!("running {} tasks...\n", TASKS.len());
    let mut rows = Vec::new();
    for task in TASKS {
        eprintln!("→ {}", task.id);
        rows.push(run_task(task).await);
    }

    let resolved = rows.iter().filter(|r| r.resolved).count();
    let total = rows.len();
    let pct = if total > 0 {
        resolved as f64 * 100.0 / total as f64
    } else {
        0.0
    };

    // Markdown report — paste-ready for the README, same spirit as eval-bench.
    println!("\n## Completion benchmark\n");
    println!("| task | status | iters | tools | in tok | out tok | ms |");
    println!("|---|---|--:|--:|--:|--:|--:|");
    for r in &rows {
        println!(
            "| {} | {} | {} | {} | {} | {} | {} |",
            r.id, r.status, r.iters, r.tool_calls, r.in_tok, r.out_tok, r.ms
        );
    }
    println!(
        "\n**pass@1 = {resolved}/{total} ({pct:.0}%)**  ·  \
         total {} in / {} out tokens",
        rows.iter().map(|r| r.in_tok).sum::<u32>(),
        rows.iter().map(|r| r.out_tok).sum::<u32>(),
    );

    // Machine-readable summary for CI / regression tracking.
    let json = serde_json::json!({
        "suite": "rust-native-v1",
        "pass_at_1": { "resolved": resolved, "total": total, "pct": pct },
        "tasks": rows.iter().map(|r| serde_json::json!({
            "id": r.id, "resolved": r.resolved, "status": r.status,
            "iters": r.iters, "tool_calls": r.tool_calls,
            "input_tokens": r.in_tok, "output_tokens": r.out_tok, "ms": r.ms,
        })).collect::<Vec<_>>(),
    });
    eprintln!("\nJSON: {}", serde_json::to_string(&json)?);

    // Non-zero exit if anything failed, so CI can gate on it.
    if resolved < total {
        std::process::exit(1);
    }
    Ok(())
}
