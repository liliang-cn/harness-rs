# harness

> **Agent = Model + Harness.** This is the *Harness* — the scaffolding that
> turns an LLM into an autonomous agent. Any domain: research, ops, assistants,
> data work, coding.

A Rust framework for production-oriented AI agents, built on the *harness
engineering* discipline (Böckeler/Thoughtworks, Lopopolo/OpenAI, 2026).
Compile-time type-safe, deterministic-first, observable. Rationale in
**[DESIGN.md](DESIGN.md)**.

## What you get

| Layer | What | Crate |
|---|---|---|
| **Models** | 3 protocol families — OpenAI-compat · Anthropic · Gemini — one `ApiKind::build(url, model, key)` | `harness-models` |
| **Tools** | fs · shell (risk-gated) · web search/fetch | `harness-tools-*` |
| **Loop** | ReAct + tool dispatch + sensor feedback + auto-fix + final synthesis | `harness-loop` |
| **Loop engineering** | recurring loops with maturity levels (L1/L2/L3), human gates, action executors, token budgets, 7 production patterns | `harness-loop-engine` |
| **Skills · Guides · Hooks · Sensors** | proc-macro registered, agentskills.io-compliant | `harness-macros`, `harness-skills` |
| **Memory · Recall** | `Memory` trait + JSONL store · cross-session search (FTS5 / CJK trigram) | `harness-core`, `harness-recall-sqlite` |
| **Scheduler · MCP** | cron-style agent jobs · MCP server + client (`rmcp`) | `harness-scheduler`, `harness-mcp` |
| **Compactor · Sandbox · Blueprint · CLI** | progressive compaction · git-worktree isolation · state machine · `harness` CLI | — |

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
Scaffold with `harness new`. See **[examples/](examples/)**.

## Loop engineering

When one agent run isn't enough — you want it to run on a cadence, verify
itself, stay on budget, and escalate when unsure — `harness-loop-engine` gives
you *trusted recurring loops*:

```rust
use harness_loop_engine::{LoopEngine, patterns};

// A daily, report-only (L1) triage loop. Maker proposes, checker verifies,
// the gate escalates, the budget caps spend, memory carries state forward.
let report = LoopEngine::new(patterns::daily_triage(), model)
    .with_maker_tool(read_only_tool)
    .run_once().await;
```

Loops earn autonomy in stages — **L1 report** → **L2 assisted** (human gates
every change) → **L3 unattended** (allowlisted actions only). After a verified
L3 approval, an `ActionExecutor` performs the project-specific side effect
(commit, PR, comment, patch, ticket update). See DESIGN.md §11.5.

## For Agents

Building or operating agents *with* harness, or landing in this repo as a
coding agent? Start here:

- **Map:** `harness-core` defines every trait (`Model`, `Tool`, `Memory`,
  `Sensor`, `Guide`, `Hook`) with zero heavy deps — read it first. Everything
  else builds on it; the dependency graph is in DESIGN.md §4.
- **Add a tool/skill:** annotate a function with `#[harness::tool]` or
  `#[skill]` — no registry edits, `inventory` collects it at link time.
  Skills follow the [agentskills.io](https://agentskills.io/specification) spec.
- **Pick a model:** always `ApiKind::{OpenAI,Anthropic,Gemini}.build(base_url,
  model, key)`. There are **no hardcoded provider URLs** — pass `base_url`
  yourself.
- **Don't burn tokens on what code can do:** lint/format/git/file-moves run
  deterministically via Sensors and Hooks, not the model. Context is scarce —
  the Compactor manages it; don't stuff everything into one prompt.
- **Isolate, don't interrupt:** permissions are burned in at sandbox spawn
  (`WorktreeSandbox` / `ContainerSandbox`), not prompted per call.
- **Run loops responsibly:** unattended loops make unattended mistakes. Keep
  `LoopSpec.intent` honest, start at L1, set a `TokenBudget`, graduate levels
  only as you build trust. Verification stays on you.

## Status

Latest: **v0.0.15** — new `harness-loop-engine` (loop engineering) + simplified,
hardcoded-URL-free model API (`ApiKind`). History in **[CHANGELOG.md](CHANGELOG.md)**.

## License

MIT OR Apache-2.0.
