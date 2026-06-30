# harness

[![crates.io](https://img.shields.io/crates/v/harness-rs.svg)](https://crates.io/crates/harness-rs) [![license](https://img.shields.io/crates/l/harness-rs.svg)](#license)

> **Agent = Model + Harness.** This is the *Harness* — the scaffolding that
> turns an LLM into an autonomous agent. Any domain: research, ops, assistants,
> data work, coding.

A Rust framework for production agents, built on the *harness engineering*
discipline (Böckeler/Thoughtworks, Lopopolo/OpenAI, 2026). Compile-time
type-safe, deterministic-first, observable, governance built in.
Full rationale in **[DESIGN.md](DESIGN.md)**.

## What you get

| Layer | What | Crate |
|---|---|---|
| **Models** | 3 protocol families (OpenAI-compat · Anthropic · Gemini), one `ApiKind::build(url, model, key)` | `harness-models` |
| **Tools** | fs · shell (risk-gated) · web search/fetch | `harness-tools-*` |
| **Loop** | ReAct + tool dispatch + sensor feedback + auto-fix | `harness-loop` |
| **Loop engineering** | recurring loops: maturity levels L1/L2/L3, human gates, action executors, token budgets | `harness-loop-engine` |
| **Orchestration** | async Run = concurrent Job DAG + retry/backoff + dynamic replanning + resumable state | `harness-orchestrator` |
| **Skills · Guides · Hooks · Sensors** | proc-macro registered, agentskills.io-compliant | `harness-macros`, `harness-skills` |
| **Memory · Recall** | `Memory` trait + JSONL store · cross-session search (FTS5 / CJK) | `harness-core`, `harness-recall-sqlite` |
| **Scheduler · MCP · Sandbox · CLI** | cron jobs · MCP server+client · git-worktree isolation · `harness` CLI | — |

## Quick start

```rust
use harness_loop::AgentLoop;
use harness_models::ApiKind;
use harness_tools_fs::{ListDir, ReadFile};
use harness_context::default_world;
use harness_core::Task;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), harness::HarnessError> {
    // One model API: protocol family + base_url + model + key. No hardcoded URLs.
    let model = ApiKind::OpenAI.build("https://api.deepseek.com", "deepseek-chat",
        std::env::var("DEEPSEEK_API_KEY").unwrap());
    let mut world = default_world(".");
    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(ReadFile))
        .with_tool(Arc::new(ListDir))
        .run(Task { description: "What is the workspace name?".into(),
                    source: None, deadline: None }, &mut world)
        .await?;
    println!("{outcome:?}");
    Ok(())
}
```

Register tools/skills/guides/sensors/hooks with `#[harness::tool]` / `#[skill]`
/ `#[guide]` / `#[sensor]` / `#[hook]` — they auto-register via `inventory`.
Scaffold a new project with `harness new`.

## Three layers, composable

- **`harness-loop`** runs *one* agent (ReAct: think → call tools → observe).
- **`harness-loop-engine`** governs a *recurring* loop: it earns autonomy in
  stages — **L1 report** → **L2 assisted** (human gates every change) → **L3
  unattended** (allowlisted actions only) — under a token budget, with an
  `ActionExecutor` for the side effect after a verified approval.
- **`harness-orchestrator`** fans *one goal* across many concurrent, dependent
  Jobs (a DAG) with retry/backoff, a run budget, crash-resumable state, and
  **dynamic replanning** (a `Planner` mutates the DAG mid-run from results).

```rust
use harness_orchestrator::{Dag, Job, Orchestrator, Run, SubagentJobRunner};

// notion/airtable/coda run concurrently; `compare` waits for all three.
let dag = Dag::from_jobs([
    Job::new("notion", "what is Notion best at?"),
    Job::new("airtable", "what is Airtable best at?"),
    Job::new("coda", "what is Coda best at?"),
    Job::new("compare", "compare them").with_deps(["notion", "airtable", "coda"]),
]);
let report = Orchestrator::new(Arc::new(SubagentJobRunner::new(model, ".")))
    .run(Run::new("run-1", "compare tools", dag)).await;
```

## Examples

See **[examples/](examples/)** — memory, recall, the scheduler, MCP, and two
end-to-end agents over a live PostgreSQL database: **`ecommerce-analyst`**
(concurrent analysis DAG) and **`ecommerce-ops-agent`** (the full stack —
dynamic replanning, L1/L2/L3 governed DB writes, cross-run memory).

## Principles

- **Don't burn tokens on what code can do** — lint/format/git run via Sensors
  and Hooks, not the model. The Compactor manages scarce context.
- **Isolate, don't interrupt** — permissions are burned in at sandbox spawn
  (`WorktreeSandbox` / `ContainerSandbox`), not prompted per call.
- **Earn autonomy in stages** — start at L1, set a budget, graduate only as you
  build trust. Unattended loops make unattended mistakes; verification is on you.

## Status

Latest: **v0.0.18**. Full history in **[CHANGELOG.md](CHANGELOG.md)**.

## License

MIT OR Apache-2.0.
