# harness

[![crates.io](https://img.shields.io/crates/v/harness-rs.svg)](https://crates.io/crates/harness-rs) [![license](https://img.shields.io/crates/l/harness-rs.svg)](#more)

> **Agent = Model + Harness.** This is the *Harness* — the scaffolding that turns
> an LLM into an autonomous agent. Any domain: research, ops, assistants, data, code.

A Rust framework for production agents. Compile-time type-safe,
deterministic-first, observable, governance built in.

- **A loop, not a call** — ReAct with tool dispatch, sensor feedback and auto-fix,
  under a budget; guards for stuck detection, acceptance, spill, deadlines.
- **Autonomy earned in stages** — L1 report → L2 assisted (human gate) → L3
  unattended (allowlist only). Graduate the agent as you build trust.
- **Honest isolation** — macOS Seatbelt and Docker *enforce*; a git worktree
  isolates changes, not capability, and reports itself that way.
- **Memory that compounds** — procedural, semantic and episodic, across sessions.
- **Code does what code can** — lint · format · git run as Sensors and Hooks,
  not model turns. Measured, not asserted.

## Quick start

```rust
use harness_core::Task;
use harness_loop::AgentLoop;
use harness_models::ApiKind;
use harness_tools_fs::{ListDir, ReadFile};
use std::sync::Arc;

// One model API: protocol family + base_url + model + key. No hardcoded URLs.
let model = ApiKind::OpenAI.build("https://api.deepseek.com", "deepseek-chat", key);
let task = Task { description: "What is the workspace name?".into(), source: None, deadline: None };
let outcome = AgentLoop::boxed(model)          // `boxed` takes Arc<dyn Model>
    .with_tool(Arc::new(ReadFile))
    .with_tool(Arc::new(ListDir))
    .run(task, &mut harness_context::default_world("."))
    .await?;
```

Register tools, skills, guides, sensors and hooks with `#[harness::tool]` /
`#[skill]` / `#[guide]` / `#[sensor]` / `#[hook]` — they auto-register via
`inventory`. Scaffold a project with `harness new`; `harness code` is an
approval-gated coding REPL built on the framework (`--yolo` to unattend).

## More

**[docs/guide.md](docs/guide.md)** — crates, composable layers, sandboxing, PII
redaction, documents/OCR, telemetry, record/replay, grounded search, examples ·
**[docs/benchmarks.md](docs/benchmarks.md)** — `pass^k`, guard ablation, cost ·
**[DESIGN.md](DESIGN.md)** · **[CHANGELOG.md](CHANGELOG.md)** · [MIT](LICENSE)
