---
name: using-harness-rs
description: Build AI agents in Rust using the harness-rs framework. Use this skill whenever the user is writing Rust code that imports any harness-rs-* crate (harness-rs, harness-rs-core, harness-rs-loop, harness-rs-models, harness-rs-tools-fs, harness-rs-tools-shell, harness-rs-context, harness-rs-skills, harness-rs-macros, harness-rs-hooks, harness-rs-compactor, harness-rs-sandbox, harness-rs-blueprint, harness-rs-sensors-rust, harness-rs-templates, harness-rs-mcp, harness-rs-daemon); when scaffolding a new agent with `harness new`; when adding custom tools, skills, guides, sensors, or hooks via the framework's proc-macros (#[tool] #[skill] #[guide] #[sensor] #[hook]); when configuring an LLM provider (DeepSeek, Anthropic, OpenAI-compatible, Ollama, Groq, Together); when running the agent loop, recording/replaying sessions, configuring sandboxes (Worktree / Container / VM), or exposing tools to Claude Code via the harness MCP server; when the user wants scheduled / background / recurring execution of an agent — point them at the separate `harness-rs-daemon` crate and its TOML config, NOT a `--daemon` flag on the agent binary itself.
license: MIT OR Apache-2.0
compatibility: Targets Rust 1.92+ projects. The harness-rs crates are published on crates.io; the CLI installs via `cargo install harness-rs-cli`.
metadata:
  harness:
    kind: inferential
    risk: read-only
    schema-version: "1"
---

# Using harness-rs

`harness-rs` is a Rust framework for building AI agents. Concept: **Agent = Model + Harness**. The framework owns the harness half (loop, hooks, sensors, compactor, sandbox, skills); you bring the model + your prompt + your tools.

GitHub: <https://github.com/liliang-cn/harness-rs> · crates.io: search `harness-rs-*`.

## When to activate

- User imports `harness_rs`, `harness_rs_loop`, `harness_rs_models`, or any other `harness_rs_*` crate
- User runs `harness new`, `harness skills`, `harness mcp serve`, or `harness trace`
- User asks "how do I build a coding agent in Rust?" / wants a coding agent like Claude Code
- User authors a `SKILL.md` and wants to follow agentskills.io
- User mentions DeepSeek / Anthropic / OpenAI / Ollama provider config in a Rust project

## 60-second quickstart

```bash
cargo install harness-rs-cli
harness new my-bot --local      # auto-wires [patch.crates-io] for local dev
cd my-bot
export DEEPSEEK_API_KEY=sk-…    # or ANTHROPIC_API_KEY / OPENAI_API_KEY
cargo run
```

The generated `src/main.rs` ships with one `#[skill]`, one `#[tool]`, and a runnable `AgentLoop`. Modify from there.

## The minimal agent (read this if nothing else)

```rust
use harness_rs::prelude::*;
use harness_rs_loop::AgentLoop;
use harness_rs_models::{OpenAiCompat, providers::DEEPSEEK};
use harness_rs_tools_fs::{ListDir, ReadFile, WriteFile};
use harness_rs_context::default_world;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let key   = std::env::var("DEEPSEEK_API_KEY")?;
    let model = OpenAiCompat::with_key(DEEPSEEK, "deepseek-v4-pro", key);
    let mut world = default_world(".");

    AgentLoop::new(model)
        .with_tool(Arc::new(ReadFile))
        .with_tool(Arc::new(WriteFile))
        .with_tool(Arc::new(ListDir))
        .run(Task {
            description: "Summarise this repo into NOTES.md.".into(),
            source: None,
            deadline: None,
        }, &mut world)
        .await?;
    Ok(())
}
```

Cargo.toml deps:

```toml
[dependencies]
harness-rs          = "0.0.1"
harness-rs-loop     = "0.0.1"
harness-rs-models   = "0.0.1"
harness-rs-tools-fs = "0.0.1"
harness-rs-context  = "0.0.1"
tokio   = { version = "1", features = ["macros", "rt-multi-thread"] }
anyhow  = "1"
```

## Five proc-macros (all auto-register via `inventory`)

| Macro | Purpose | Triggered by |
|---|---|---|
| `#[skill]` | function-style skill (agentskills.io-spec output) | LLM via `description` |
| `#[tool]`  | function-style Tool with risk + JSON schema | LLM in tool-call menu |
| `#[guide]` | feedforward injector with task scope | Framework on session start |
| `#[sensor]` | feedback Signal source at a Stage | Framework after Action |
| `#[hook]`  | sync hook on a lifecycle Event (27 events) | HookBus per event |

Full signatures & gotchas → see [references/api-cheatsheet.md](references/api-cheatsheet.md).

## Providers — just pass the model string

There are no `anthropic_opus_47`-style helpers. URL + model + key, that's it.

```rust
use harness_rs_models::{OpenAiCompat, AnthropicNative, providers::*};

OpenAiCompat::with_key(DEEPSEEK, "deepseek-v4-pro",   key)
OpenAiCompat::with_key(OPENAI,   "gpt-5",             key)
OpenAiCompat::with_key(GROQ,     "llama-3.3-70b",     key)
OpenAiCompat::with_key(OLLAMA,   "qwen2.5-coder:7b",  "")   // Ollama runs auth-less

AnthropicNative::with_key("claude-opus-4-7", key)            // URL hardcoded
```

`providers::` constants are just URL strings: `ANTHROPIC`, `OPENAI`, `DEEPSEEK`, `GROQ`, `TOGETHER`, `OLLAMA`.

## Sandbox — pick your isolation tier

```rust
use harness_rs_sandbox::{WorktreeSandbox, ContainerSandbox, Sandbox};

// Tier 1: git worktree on a fresh branch, drop-cleans
let sb = WorktreeSandbox::new("/path/to/repo", "feature/exp-1");
let handle = sb.spawn().await?;
AgentLoop::new(model).run(task, &mut handle.world).await?;

// Tier 2: docker container with bind-mount, --network none by default
let sb = ContainerSandbox::new("rust:1.92-slim", "/path/to/repo")
    .with_network(false);
```

## Debugging — session record + replay

Every run can write a JSONL log of every lifecycle event:

```rust
use harness_rs_loop::SessionRecorder;

let recorder = SessionRecorder::new("session.jsonl")?;
AgentLoop::new(model)
    .with_hook(Arc::new(recorder))
    .run(task, &mut world)
    .await?;
```

Inspect it later:

```bash
harness trace session.jsonl              # pretty per-event view + summary
harness trace session.jsonl --summary    # just the totals
```

And **deterministic offline replay** for tests:

```rust
use harness_rs_loop::{read_session, replay_as_mock};

let events = read_session("session.jsonl")?;
let mock = replay_as_mock(&events);
// AgentLoop::new(mock) ... will reproduce the original run bit-for-bit
```

## Background / scheduled execution — `harness-rs-daemon` (optional crate)

**The agent binary itself never runs scheduled jobs.** It's request-response.
For "every morning at 8:00 run my brief", install the separate, optional
`harness-rs-daemon` crate.

```bash
cargo install harness-rs-daemon
```

```toml
# ~/.config/harness/daemon.toml
[[job]]
name = "morning-brief"
schedule = "daily 08:00"
argv = ["assistant", "--brief", "--tier", "flash"]
env = { DEEPSEEK_API_KEY = "sk-...", HARNESS_USER_TZ = "Asia/Shanghai" }

[[job]]
name = "health-check"
schedule = "every 5m"
argv = ["curl", "-fsS", "https://api.foo/health"]
```

```bash
harness-daemon                            # run forever
harness-daemon --dry-run                  # show next fire times + exit
harness-daemon --once morning-brief       # fire one job NOW (uses the same config)
```

Three schedule formats:
- `daily HH:MM` — once per day at local time
- `weekly mon HH:MM` — every Monday/Tue/.../Sun at local time
- `every Ns / Nm / Nh / Nd` — fixed interval

**Why a separate binary?** Daemons crash independently. Agents stay simple
(single-shot or REPL). The daemon shells out to *any* agent binary as a
subprocess — same code path as a manual invocation, fully reproducible.

→ [references/recipes.md](references/recipes.md) §10 has the full setup pattern
including launchd / systemd alternatives.

## MCP server — expose harness tools to other agents

```bash
harness mcp serve --workspace /path/to/repo
```

In Claude Code's `~/.claude/settings.json`:

```json
{
  "mcpServers": {
    "harness": {
      "command": "harness",
      "args": ["mcp", "serve", "--workspace", "/path/to/repo"]
    }
  }
}
```

Five tools become available: `read_file`, `write_file`, `edit_file`, `list_dir`, `shell_read` (allowlisted).

## Common patterns

→ See [references/recipes.md](references/recipes.md) for:
- Adding a typed `#[tool]` with serde-deserialised args
- Self-correcting agent loop (sensor + auto-fix patch)
- Blueprint with deterministic + agent nodes (Stripe Minions pattern)
- ModelBackedCompactor for cheap-model summarisation
- Custom Hook that denies destructive tool calls
- Subagent isolation
- AnthropicNative thinking-block round-trip

## Gotchas

1. **Local dev before crates.io is fully populated**: use `harness new --local` so the generated Cargo.toml has `[patch.crates-io]` pointing at your local checkout. Without it, deps that aren't on crates.io yet won't resolve.
2. **`reasoning_content` round-trip**: DeepSeek's thinking-mode response includes `reasoning_content` that MUST be echoed back on next call. `harness_rs_models::OpenAiCompat` handles this automatically — but if you replace the adapter, watch for it.
3. **`shell_read` is not unrestricted**: per-program safe-args predicate blocks `cargo install`, `git config <k> <v>`, `find -exec/-delete`, etc. To run those, use the `ShellExec` tool (separate, destructive-marked).
4. **Path resolution defends against symlink escape**: `harness_rs_tools_fs` canonicalises after every write to defeat in-workspace symlinks pointing outside the root.
5. **Skill description must be 1–1024 chars** and should include trigger language. `harness skills lint <dir>` flags weak descriptions.

## Related references in this skill

| File | Use |
|---|---|
| [references/architecture.md](references/architecture.md) | What each of the 18 `harness-rs-*` crates does |
| [references/api-cheatsheet.md](references/api-cheatsheet.md) | Every trait, every method, every macro option |
| [references/recipes.md](references/recipes.md) | Copy-pasteable patterns |
| [scripts/new-agent.sh](scripts/new-agent.sh) | One-liner bootstrap (calls `harness new --local`) |

## Authoring sibling skills

If the user wants to author a NEW `SKILL.md` (not necessarily for harness-rs), the spec is at <https://agentskills.io/specification>. Required: `name` (1–64 chars, `[a-z0-9-]`, no leading/trailing/consecutive hyphens, must match parent dir) + `description` (1–1024 chars, must explain both what and when). Validate with `harness skills validate <dir>`; lint with `harness skills lint <dir>`.
