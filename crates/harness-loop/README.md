# harness-rs-loop

[![crates.io](https://img.shields.io/crates/v/harness-rs-loop.svg)](https://crates.io/crates/harness-rs-loop)

The **ReAct agent loop** for harness-rs: think → call tools → observe →
self-correct, repeat until the model stops or the iteration budget runs out.
Also home to **subagent isolation** and **session record/replay**.

## What the loop does each iteration

1. Applies `Guide`s (system context) — once at the start.
2. Sends the `Context` (with available tools) to the `Model`.
3. Dispatches every returned tool call through the `ToolRegistry`.
4. Runs `Sensor`s after each action — auto-fix patches are applied to the
   `World` directly; blocking signals are fed back to the model to retry.
5. `Hook`s wrap each step (PreToolUse / PostToolUse / TaskCompleted).
6. Stops when the model returns no tool calls, or `max_iters` is hit.

## Usage

```rust,ignore
use harness_loop::{AgentLoop, Outcome};
use harness_models::OpenAiCompat;
use harness_context::default_world;
use harness_core::Task;
use harness_tools_fs::{ReadFile, ListDir};
use std::sync::Arc;

let model = OpenAiCompat::with_key("https://api.deepseek.com", "deepseek-chat", key);
let mut world = default_world(".");

let outcome = AgentLoop::new(model)
    .with_tool(Arc::new(ReadFile))
    .with_tool(Arc::new(ListDir))
    .run_with_max_iters(
        Task { description: "summarize the workspace".into(), source: None, deadline: None },
        &mut world, 12,
    )
    .await?;

match outcome {
    Outcome::Done { text, iters, tools_called, usage, .. } =>
        println!("done in {iters} iters, {tools_called} tools, {} tok", usage.input_tokens),
    Outcome::BudgetExhausted { last_text, .. } => println!("hit budget: {last_text:?}"),
}
```

`Outcome` is `#[non_exhaustive]` — always destructure with a trailing `..`.

## Subagents

Run an isolated child agent with its own tools and budget, returning a single
report to the parent — the basis for `harness-rs-orchestrator` and
`harness-rs-scheduler`:

```rust,ignore
use harness_loop::{Subagent, SubagentSpec};

let spec = SubagentSpec::new("researcher", task).with_max_iters(8).with_tool(tool);
let report = Subagent::new(harness_core::DynModel(model), spec).run(&mut world).await?;
println!("{:?}", report.text);
```

## Record & replay

Every run can emit a JSONL session log; `read_session` + `SessionStats` /
`format_event_*` reconstruct it (the `harness trace` CLI command reads these).

## License

MIT OR Apache-2.0.
