# harness-rs

[![crates.io](https://img.shields.io/crates/v/harness-rs.svg)](https://crates.io/crates/harness-rs)

The **facade** crate for the harness-rs agent framework — depend on this one and
you get the whole public surface: the core traits/types (`harness-rs-core`), the
`#[tool]` / `#[skill]` / `#[guide]` / `#[sensor]` / `#[hook]` proc-macros
(`harness-rs-macros`), and skills under the `skills` module.

> **Agent = Model + Harness.** The *model* is the LLM. The *harness* is
> everything around it: what it can see (`Guide`), what it can call (`Tool`),
> what feedback comes back (`Sensor`), what policy wraps each step (`Hook`), and
> how context stays small (`Compactor`). The [`AgentLoop`](https://docs.rs/harness-rs-loop)
> ties them into a ReAct loop with self-correction.

## Install

```toml
[dependencies]
harness-rs         = "0.0.21"
harness-rs-loop    = "0.0.21"
harness-rs-models  = "0.0.21"
harness-rs-tools-fs = "0.0.21"
harness-rs-context = "0.0.21"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

## Quick start

Define a tool with `#[tool]`, point the model adapter at any OpenAI-compatible
endpoint, and run the loop:

```rust,ignore
use harness::{tool, ToolError};
use harness_loop::AgentLoop;
use harness_models::OpenAiCompat;
use harness_context::default_world;
use harness_core::Task;
use std::sync::Arc;

/// Add two integers.
#[tool(name = "add", risk = "Safe")]
async fn add(a: i64, b: i64) -> Result<i64, ToolError> {
    Ok(a + b)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let model = OpenAiCompat::with_key(
        "https://api.deepseek.com", "deepseek-chat",
        std::env::var("DEEPSEEK_API_KEY")?,
    );
    let mut world = default_world(".");
    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(add()))
        .run(Task { description: "what is 2 + 2?".into(), source: None, deadline: None },
             &mut world)
        .await?;
    println!("{outcome:?}");
    Ok(())
}
```

Macros auto-register via `inventory`, so `#[skill]` / `#[tool]` items are
discoverable without a central registry. Scaffold a project with `harness new`.

## Where to go next

| You want to… | Crate |
|---|---|
| The ReAct loop, subagents, session replay | [`harness-rs-loop`](https://docs.rs/harness-rs-loop) |
| Model adapters (OpenAI-compat · Anthropic · Gemini) | [`harness-rs-models`](https://docs.rs/harness-rs-models) |
| Recurring/governed loops (L1/L2/L3) | `harness-rs-loop-engine` |
| Concurrent Job DAG + replanning | `harness-rs-orchestrator` |
| Episodic learning + semantic recall | `harness-rs-experience`, `harness-rs-cortexdb` |

Full design rationale: **DESIGN.md** in the [workspace repo](https://github.com/liliang-cn/harness-rs).

## License

MIT OR Apache-2.0.
