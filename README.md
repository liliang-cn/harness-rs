# harness

> **Agent = Model + Harness.** This crate is the *Harness* — the modular
> scaffolding that turns an LLM into an autonomous agent (any domain — research,
> ops, assistants, data work, coding — not coding-only).

A Rust framework for building production agents, based on the *harness
engineering* discipline (Böckeler / Thoughtworks and Lopopolo / OpenAI, 2026).
Full architectural rationale in **[DESIGN.md](DESIGN.md)**.

## What you get

| Layer | What | Crate |
|---|---|---|
| **Models** | OpenAI-compatible · Anthropic · Gemini · scriptable mock | `harness-models` |
| **Tools** | fs · shell (risk-gated) · web search/fetch | `harness-tools-fs/shell/web` |
| **Loop** | ReAct + tool dispatch + sensor feedback + auto-fix + final-synthesis | `harness-loop` |
| **Skills · Guides · Hooks · Sensors** | proc-macro registered; agentskills.io-compliant | `harness-macros`, `harness-skills` |
| **Memory** | `Memory` trait + JSONL store + cheap-model distillation | `harness-core`, `harness-context` |
| **Recall** | cross-session search (FTS5 / CJK trigram), owner-scoped | `harness-recall-sqlite` |
| **Learning loop** | self-evolving skills + memory at session end | `harness-loop`, `harness-tools-skills` |
| **Scheduler** | cron-style agent jobs + delivery (stdout / email) | `harness-scheduler` |
| **MCP** | server (expose tools) + client (consume any MCP server, via `rmcp`) | `harness-mcp`, `harness-mcp-client` |
| **Compactor · Sandbox · Blueprint · Observability · CLI** | progressive compaction · git-worktree isolation · state machine · JSONL traces · `harness` CLI | — |

## Quick start

```rust
use harness_loop::AgentLoop;
use harness_models::{OpenAiCompat, providers::DEEPSEEK};
use harness_tools_fs::{ListDir, ReadFile};
use harness_context::default_world;
use harness_core::Task;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), harness::HarnessError> {
    let model = OpenAiCompat::with_key(DEEPSEEK, "deepseek-chat",
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

Register tools/skills/guides/sensors/hooks with `#[harness::tool]` /
`#[skill]` / `#[guide]` / `#[sensor]` / `#[hook]` — they auto-register via
`inventory`. Scaffold a project with `harness new`.

See **[examples/](examples/)** for memory, recall, the learning loop, the
scheduler, MCP, and end-to-end runs against a real model.

## Status

Latest: **v0.0.7** — adds `harness-rs-mcp-client` (consume any MCP server's
tools). Full history in **[CHANGELOG.md](CHANGELOG.md)**.

## License

MIT OR Apache-2.0.
