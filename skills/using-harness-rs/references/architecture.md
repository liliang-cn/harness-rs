# harness-rs architecture

18 crates, one responsibility each. Dependency direction is downward.

```
┌───────────────────────────────────────────────────────────────┐
│  harness-rs (facade)        — re-exports the public surface   │
│  harness-rs-cli             — `harness` binary                 │
│  harness-rs-templates       — pre-built Blueprints             │
└──────────────┬────────────────────────────────────────────────┘
               │
┌──────────────┴───────────────────────────────────────────────┐
│  harness-rs-loop            — AgentLoop + Subagent + replay   │
│  harness-rs-blueprint       — state machine executor          │
│  harness-rs-mcp             — JSON-RPC stdio MCP server       │
└──────────────┬───────────────────────────────────────────────┘
               │
┌──────────────┴───────────────────────────────────────────────┐
│  harness-rs-models          — OpenAiCompat/Anthropic/Mock     │
│  harness-rs-compactor       — 5-stage compaction              │
│  harness-rs-sandbox         — Worktree/Container/VM           │
│  harness-rs-skills          — SKILL.md spec validator         │
│  harness-rs-hooks           — HookBus, OpenTelemetry feature  │
│  harness-rs-context         — Default World runtime           │
│  harness-rs-tools-fs        — read/write/edit/list (jail-safe)│
│  harness-rs-tools-shell     — allowlisted shell_read          │
│  harness-rs-sensors-rust    — cargo check / clippy            │
│  harness-rs-sensors-common  — shared sensor scaffolding       │
│  harness-rs-macros          — #[skill] #[tool] #[guide] …     │
└──────────────┬───────────────────────────────────────────────┘
               │
       ┌───────┴────────┐
       │ harness-rs-core │   — Model/Tool/Guide/Sensor/Hook traits +
       └─────────────────┘     Context/World/Block/Event/27 events
```

## Crate selection guide

If you're building...

| ...this | Pull in |
|---|---|
| A minimal CLI agent that reads files | `harness-rs` + `harness-rs-loop` + `harness-rs-models` + `harness-rs-tools-fs` + `harness-rs-context` |
| ...that also runs `cargo check` | + `harness-rs-sensors-rust` |
| ...with a recorded session log | + `SessionRecorder` from `harness-rs-loop` (no extra crate) |
| ...isolated in a git worktree | + `harness-rs-sandbox` |
| ...as a multi-step deterministic + LLM pipeline | + `harness-rs-blueprint` |
| ...with sub-agents spawning sub-agents | + `Subagent` from `harness-rs-loop` |
| ...that exposes tools to Claude Code via MCP | + `harness-rs-mcp` (or just run `harness mcp serve`) |
| Only writing a custom Model adapter | `harness-rs-core` alone — implement `harness_rs_core::Model` |
| Only writing skills (no agent code) | `harness-rs-skills` alone — for validator/loader API |

## Pure dependency-light crates

- `harness-rs-core` deliberately depends on only `serde`, `async-trait`, `thiserror`, `futures`, `inventory`, `serde_json`. No tokio, no reqwest. Safe to embed in WASM/no_std-ish contexts (with `default-features = false`).
- `harness-rs-macros` is a proc-macro crate; depends on `syn` + `quote` host-side. Its generated code references `harness_rs_core::__export::*`.

## What is NOT in `harness-rs-core`

- No model adapters (those are in `harness-rs-models`)
- No tokio runtime (consumed in `harness-rs-context`)
- No HTTP client (in `harness-rs-models`)
- No file I/O (in `harness-rs-tools-fs`)

Keeps the trait surface portable.

## Cross-crate type traffic

`harness-rs-core` exports the vocabulary every other crate speaks:

- `Model`, `ModelOutput`, `Tool`, `ToolResult`, `ToolSchema`, `ToolRisk`
- `Guide`, `GuideScope`, `Sensor`, `Stage`, `Signal`, `Severity`, `FixPatch`
- `Hook`, `HookOutcome`, `Event<'a>` (27 variants)
- `Compactor`, `CompactionStage`, `Budget`
- `Skill`, `SkillManifest`, `SkillHandler`, `Resource`, `HarnessExt`
- `Context`, `Block`, `Turn`, `TurnRole`, `Task`, `Policy`, `World`
- `Action`, `Execution`, `NotificationKind`, `SessionSource`, `SubagentStatus`
- error enums: `HarnessError`, `ModelError`, `ToolError`, `GuideError`, `SensorError`, `CompactError`, `SkillError`

All marked `#[non_exhaustive]` so adding variants won't break downstream matches.
