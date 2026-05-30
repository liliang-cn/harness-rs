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
| **Tools** | fs (read/write/edit/list), shell (risk-classified allowlist), web (DDG → Bing search + URL fetch + HTML→text) | `harness-tools-fs`, `harness-tools-shell`, `harness-tools-web` |
| **Sensors** | `cargo check` + `cargo clippy` produce LLM-friendly `Signal`s; auto-fix patches apply automatically | `harness-sensors-rust` |
| **Skills** | strict [agentskills.io](https://agentskills.io/specification) validator + `#[skill]` proc-macro + export to spec-compliant directory | `harness-skills`, `harness-macros` |
| **Guides** | feedforward Markdown context, scoped by task | `#[guide]` + `harness-templates` |
| **Hooks** | 27-event lifecycle bus with deny/inject/mutate | `harness-hooks` + `#[hook]` |
| **Compactor** | 5-stage progressive compaction (auto-triggered by budget) | `harness-compactor` |
| **Loop** | ReAct + tool-call dispatch + sensor feedback + auto-fix + forced final-synthesis on budget exhaustion | `harness-loop` |
| **Memory** | Open `Memory` trait + JSONL `FileMemory` + `MemoryGuide` (recall) + `MemorySynthesizer` (cheap-model distillation into atomic facts) | `harness-core`, `harness-context`, `harness-loop` |
| **Recall** | Cross-session conversation search — `RecallStore` trait + JSONL `FileRecall` (default) + FTS5/CJK-trigram `SqliteRecall`; one-call `.with_recall(store)` adds capture + a `session_search` tool, owner-scoped | `harness-core`, `harness-context`, `harness-recall-sqlite` |
| **Learning loop** | Self-evolving skills + memory — `.with_learning_loop(cfg)` forks a review subagent at session end that writes/patches skills (`skill_manage`) and memory from the transcript | `harness-loop`, `harness-tools-skills` |
| **Scheduler** | In-process scheduled agent jobs with delivery — `JobStore` + `Channel` (stdout / email-Resend) + a `Scheduler` that runs jobs as agent turns + a `cronjob` tool for self-scheduling | `harness-scheduler` |
| **Observability** | `SessionRecorder` JSONL traces, `LiveProgressHook` live stderr stream, `harness trace --verbose` | `harness-loop` + `harness-cli` |
| **Blueprint** | deterministic + agent state machine with retry/fallback | `harness-blueprint` |
| **Sandbox** | git worktree isolation (container/VM in v0.2) | `harness-sandbox` |
| **CLI** | `harness skills validate / list / export`, `harness new`, `harness trace` | `harness-cli` |

## 60-second tour

### 1. Build a minimal agent

```rust
use harness::prelude::*;
use harness_loop::AgentLoop;
use harness_models::{OpenAiCompat, providers::DEEPSEEK};
use harness_tools_fs::{ListDir, ReadFile};
use harness_context::default_world;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), harness::HarnessError> {
    let model = OpenAiCompat::with_key(
        DEEPSEEK,
        "deepseek-chat",
        std::env::var("DEEPSEEK_API_KEY").unwrap(),
    );
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

### 6. Open long-term memory (your harness, your memory)

Persist durable facts across sessions, recall them automatically on the
next run, all on the user's disk — no provider-side state.

```rust
use harness_context::FileMemory;
use harness_loop::{MemoryGuide, MemorySynthesizer};
use std::sync::Arc;

let mem: Arc<dyn harness::Memory> =
    Arc::new(FileMemory::open("~/.my-agent/memory.jsonl")?);

// Cheap "synth" model distils each session into 1-3 atomic facts.
let synth_model: Arc<dyn harness::Model> = Arc::new(OpenAiCompat::with_key(
    DEEPSEEK, "deepseek-v4-flash", key.clone(),
));
let synth = Arc::new(
    MemorySynthesizer::new(mem.clone(), synth_model)
        .with_source("my-agent")
        .with_max_facts(3),
);

let loop_ = AgentLoop::new(model)
    .with_guide(Arc::new(MemoryGuide::new(mem.clone()).with_top_k(5)))
    .with_hook(synth.clone() as Arc<dyn harness::Hook>);

// ... run sessions ...

synth.flush_pending().await; // before main() exits
```

What you get:
- `MemoryGuide` calls `recall()` with the current task description on every
  session start and injects the top-K hits into the model's system prompt.
- `MemorySynthesizer` asks the cheap synth model to extract durable facts
  from each completed session, parses them as JSON, persists each as an
  independent `MemoryEntry`. Markdown fences tolerated; unparseable output
  falls back to a `"synth-raw"` entry rather than silent drop.
- File format is plain JSONL — `cat`, `grep`, version-controllable,
  transferable. Swap to a vector store by implementing the `Memory` trait;
  nothing else in the framework needs to change.

The `examples/personal-assistant` and `examples/investor-bot` binaries
expose this via `--memory <path>` + `--synth-model <id>` flags.

### 7. Observe what the agent is doing

```bash
# Live stderr stream of every model call, tool call, and tool result:
HARNESS_PROGRESS=1 cargo run -p investor-bot -- "..."

# Or with a recorded session log + post-mortem inspection:
cargo run -p investor-bot -- --record /tmp/run.jsonl "..."
harness trace /tmp/run.jsonl --verbose
```

## Testing & verification

```
$ cargo test --workspace
... 133 tests passing
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

See [`examples/README.md`](examples/README.md) for full descriptions. In order
of increasing surface area:

- `examples/deepseek-hello` — smallest possible Hello-world against DeepSeek.
- `examples/crate-keeper` — `MockModel` smoke test; no network.
- `examples/personal-assistant` — scheduling agent with `UserProfile`,
  REPL, brief mode.
- `examples/investor-bot` — autonomous web research with multi-engine search
  fallback + retry.

## Status

- **v0.0.1** — initial publish (15 of 18 crates).
- **v0.0.2** — `UserProfile` + `ProfileGuide`, optional `harness-rs-daemon`
  scheduler, retry/backoff in model adapters, MCP server with resources +
  prompts, session record/replay, multi-engine search, `#[non_exhaustive]`
  sweep, security gates on `FixPatch::RunCommand` + `shell_read`. (Shipped
  in stages; superseded by 0.0.3.)
- **v0.0.3** — Re-publish of the 0.0.2 feature set as a single consistent
  snapshot. No new features.
- **v0.0.4** — Observability (`LiveProgressHook`,
  `harness trace --verbose`), forced final-synthesis on budget exhaustion,
  and **open long-term memory** (`Memory` trait, `FileMemory` JSONL,
  `MemoryGuide`, `MemorySynthesizer` cheap-model distillation). Examples
  ship `--memory` / `--synth-model` / `--progress` / `--record` /
  `HARNESS_*` env vars.
- **v0.0.5** — ✅ **current**. Three new opt-in, one-builder-call capabilities:
  **cross-session recall** (`RecallStore` + `FileRecall` + `session_search`;
  optional FTS5 `harness-rs-recall-sqlite`), a **self-evolving learning loop**
  (`.with_learning_loop()` forks a review subagent to write/patch skills +
  memory; new `harness-rs-tools-skills` `skill_manage`), and **in-process
  scheduling + delivery** (new `harness-rs-scheduler`: `JobStore`, `Channel`,
  `cronjob`). Plus `harness_core::DynModel` (use a boxed `Arc<dyn Model>` as a
  concrete `M`). Verified with a real-DeepSeek end-to-end
  (`examples/deepseek-caps-e2e`). See [CHANGELOG](CHANGELOG.md).
- **v0.1+** — `ContainerSandbox` / `VmSandbox` / first-class blueprint
  `Node::Agent` / semantic memory backends are on the road.

## License

Dual-licensed under MIT OR Apache-2.0.
