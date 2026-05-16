# harness

> **Agent = Model + Harness.** This crate is the *Harness* — the modular
> scaffolding around an LLM that turns it into an autonomous coding agent.

Rust framework for building production coding agents, based on the
*harness engineering* discipline as written up by Böckeler (Thoughtworks,
2026) and Lopopolo (OpenAI, 2026). See **`DESIGN.md`** for the full
architectural rationale.

## What you get

| Layer | What it does | Crate |
|------|-----|-----|
| **Model** | OpenAI-compatible + Anthropic-native + scriptable mock | `harness-models` |
| **Tools** | fs (read/write/edit/list), shell (risk-classified allowlist) | `harness-tools-fs`, `harness-tools-shell` |
| **Sensors** | `cargo check` + `cargo clippy` produce LLM-friendly `Signal`s; auto-fix patches apply automatically | `harness-sensors-rust` |
| **Skills** | strict [agentskills.io](https://agentskills.io/specification) validator + `#[skill]` proc-macro + export to spec-compliant directory | `harness-skills`, `harness-macros` |
| **Guides** | feedforward Markdown context, scoped by task | `#[guide]` + `harness-templates` |
| **Hooks** | 27-event lifecycle bus with deny/inject/mutate | `harness-hooks` + `#[hook]` |
| **Compactor** | 5-stage progressive compaction (auto-triggered by budget) | `harness-compactor` |
| **Loop** | ReAct + tool-call dispatch + sensor feedback + auto-fix | `harness-loop` |
| **Blueprint** | deterministic + agent state machine with retry/fallback | `harness-blueprint` |
| **Sandbox** | git worktree isolation (container/VM in v0.2) | `harness-sandbox` |
| **CLI** | `harness skills validate / list / export`, `harness new` | `harness-cli` |

## 60-second tour

### 1. Build a minimal agent

```rust
use harness::prelude::*;
use harness_loop::AgentLoop;
use harness_models::{OpenAiCompat, providers};
use harness_tools_fs::{ListDir, ReadFile};
use harness_context::default_world;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), harness::HarnessError> {
    let model = OpenAiCompat::new(providers::deepseek_flash(
        std::env::var("DEEPSEEK_API_KEY").unwrap(),
    ));
    let mut world = default_world(".");
    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(ReadFile))
        .with_tool(Arc::new(ListDir))
        .run(
            Task { description: "Find Cargo.toml and tell me the workspace name".into(),
                   source: None, deadline: None },
            &mut world,
        )
        .await?;
    println!("{outcome:?}");
    Ok(())
}
```

### 2. Register a skill, tool, guide, sensor, or hook with a proc-macro

```rust
/// Greet the user politely. Use when the user explicitly asks for a friendly hello.
#[harness::skill(name = "polite-hello", harness(kind = "inferential", risk = "read-only"))]
async fn polite_hello(_ctx: &mut Context, _w: &mut World) -> Result<(), harness::SkillError> {
    Ok(())
}

#[harness::tool(name = "reverse", risk = "read-only",
    schema = r#"{"type":"object","properties":{"text":{"type":"string"}}}"#)]
async fn reverse(args: serde_json::Value, _w: &mut World)
    -> Result<ToolResult, ToolError> { /* ... */ }

#[harness::guide(scope = "always", kind = "inferential")]
async fn project_intro(ctx: &mut Context, _w: &World) -> Result<(), GuideError> {
    ctx.guides.push(Block::Text("Always reply in two sentences.".into()));
    Ok(())
}

#[harness::sensor(stage = "self-correct", kind = "computational")]
async fn no_unwrap(action: &Action, w: &World) -> Result<Vec<Signal>, SensorError> { /* ... */ }

#[harness::hook(event = "PreToolUse", name = "audit")]
fn audit(ev: &Event<'_>, _w: &mut World) -> HookOutcome {
    tracing::info!(?ev); HookOutcome::Allow
}
```

All five auto-register via `inventory`; `AgentLoop::with_macro_hooks()` and
`SkillRegistry::with_macro_skills()` pick them up at runtime.

### 3. Hybrid deterministic + agent state machine

```rust
use harness_blueprint::{Blueprint, Node, NodeOutput, Transition};

let bp = Blueprint::new()
    .add("fmt",    Node::deterministic(|w| Box::pin(async move {
        w.runner.exec("cargo", &["fmt", "--all"], Some(w.repo.root.as_path())).await?;
        Ok(NodeOutput { transition: Transition::Next, data: Default::default() })
    })))
    .add("work",   Node::agent(|w| Box::pin(async move { /* run AgentLoop */ })))
    .add("test",   Node::deterministic(|w| Box::pin(async move { /* cargo test */ })))
    .edge("fmt", "work").edge("work", "test")
    .branch_on_failure("test", "work", 2);
```

### 4. Validate / export skills for any spec-compliant agent

```bash
$ harness skills validate ./skills/format-rust
✓ valid: format-rust — Run cargo fmt across the workspace.

$ harness skills export ./out --from ./skills
✓ ./out/format-rust/SKILL.md
✓ ./out/review-axum/SKILL.md
exported 2 skill(s) to ./out
```

The exported directory is consumable by Claude Code, Cursor, Codex, or any
agent that follows the agentskills.io spec.

### 5. Scaffold a new agent project

```bash
$ harness new my-agent
✓ created my-agent/
  └─ Cargo.toml
  └─ src/main.rs   # minimal agent with one tool and one skill
```

## Testing & verification

```
$ cargo test --workspace
... 70+ tests passing
```

Three layers of verification:

1. **Unit tests** (per crate) — pure logic, no I/O.
2. **AgentLoop integration tests** (`harness-loop/tests/agent_loop.rs`) —
   `MockModel` drives the full pipeline with scripted responses; zero
   network, deterministic.
3. **Golden-path test** (`harness-loop/tests/golden_path.rs`) — every
   component (guide, tool, sensor, auto-fix, hook, compactor) exercised at
   once against a tmp workspace, final on-disk state asserted.
4. **Live demo** (`examples/crate-keeper`) — runs against DeepSeek
   (`flash` or `pro` tier) for wire-format validation that mocks can't
   catch.

## Examples

- `examples/deepseek-hello` — smoke-test the `Model` trait against DeepSeek
- `examples/crate-keeper` — read-only audit of any Rust workspace; produces a
  `HARNESS_NOTES.md` summary

## Status

Per **DESIGN.md §15**:

- **v0.0.1 MVP** — ✅ complete
- **v0.1** — ✅ complete + production-hardened (security, validation parity,
  `#[non_exhaustive]` on stable enums, full Anthropic round-trip incl. thinking)
- **v0.2+** — VmSandbox / ContainerSandbox / MCP server / OpenTelemetry /
  session replay — deferred

## License

Dual-licensed under MIT OR Apache-2.0.
