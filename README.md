# harness

[![crates.io](https://img.shields.io/crates/v/harness-rs.svg)](https://crates.io/crates/harness-rs) [![license](https://img.shields.io/crates/l/harness-rs.svg)](#more)

harness-rs is a Rust framework for building AI agents that run in production:
assistants, research and data agents, ops automation, coding agents, BI and
process agents for a business. You bring a model and the tools for your domain;
the framework supplies everything around the model that makes it behave like a
dependable program instead of a chat window.

That "everything around the model" is the *harness*. An agent is a model plus a
harness, and most of what decides whether an agent works — whether it stops,
whether it is right, what it costs, whether anyone can tell afterwards what it
did — lives in the harness, not in the model:

- **Guides** shape the request before the model sees it (feed-forward): rules,
  boundaries, the user's profile, recalled memory.
- **Sensors** check the work after each action (feedback): lint, format, tests,
  anything code can verify. Blocking signals go back to the model; mechanical
  fixes are applied directly as patches, without spending a model turn.
- **Hooks** observe and govern every lifecycle event — they can deny a tool
  call, inject text, or just record what happened.
- **Acceptance checks** decide when the work is actually done. The model
  stopping is its opinion; the check is the fact.
- **Guards** keep the run bounded: a token budget with staged compaction,
  stuck and monotony detection, per-call tool deadlines, a result-size ceiling
  that spills oversized payloads to disk, cancellation.

What sets it apart:

- **A loop, not a call** — ReAct with tool dispatch, sensor feedback and auto-fix,
  under a budget; guards for stuck detection, acceptance, spill, deadlines.
- **Autonomy earned in stages** — L1 report → L2 assisted (human gate) → L3
  unattended (allowlist only). Graduate the agent as you build trust.
- **Honest isolation** — macOS Seatbelt, Linux Bubblewrap and containers
  *enforce*; a git worktree isolates changes, not capability, and reports
  itself that way.
- **Memory that compounds** — procedural, semantic and episodic, across sessions,
  with the backend chosen by a URL.
- **Code does what code can** — lint · format · git run as Sensors and Hooks,
  not model turns. Measured, not asserted.
- **Compile-time type safety** — tools, skills, guides, sensors and hooks are
  plain Rust types registered by proc-macro; structured output can be typed
  (`run_typed::<T>`).

## How a run works

One call to `AgentLoop::run` takes a `Task` and returns an `Outcome`. In between:

```text
  Task
   │
   ▼
 Context ── system prompt · tool schemas · history · metadata · policy
   │        (context budget sized from the model's context window)
   ▼
 Guides ── applied once, before the first model call (feed-forward)
   │
   ▼
 ┌─────────────────────────── each iteration (≤ max_iters) ──────────────────────┐
 │                                                                               │
 │  Compaction ── staged as the context fills: 60% … 95% of the input budget     │
 │      │                                                                        │
 │      ▼                                                                        │
 │  Model ── complete() or stream()  ─────────────▶  ModelOutput                 │
 │      │                                                │                       │
 │      │                                no tool calls ──┤                       │
 │      │                                                ▼                       │
 │      │                               Acceptance checks (+ sealed contract)    │
 │      │                                  fail ─▶ reason fed back, loop again   │
 │      │                                  pass ─▶ Outcome::Done ────────────────┼─▶
 │      ▼ tool calls                                                             │
 │  Tools ── leading read-only calls prefetched in parallel                      │
 │      │    each call: PreToolUse hook (may deny) → dispatch under deadline     │
 │      │    → result ceiling / spill to disk → PostToolUse hook                 │
 │      ▼                                                                        │
 │  Sensors ── signals back to the model · auto-fix patches applied to the world │
 │      │                                                                        │
 │      ▼                                                                        │
 │  Stuck / monotony detection ── spinning ─▶ forced answer · Outcome::Stuck     │
 │                                                                               │
 └── max_iters reached ─▶ forced final answer (tools off), Outcome::BudgetExhausted
     cancellation token ─▶ Outcome::Cancelled (partial text kept)
```

Every step above fires a lifecycle `Event` (`SessionStart`, `PreModel`,
`PostToolUse`, `PreCompact`, `AcceptanceChecked`, `BudgetWarning`, …) through
one hook bus. The loop's own behaviour and everything that watches it hang off
that same seam:

```text
 AgentLoop ── Event ──▶ HookBus   first Deny wins · Inject payloads concatenate
                          │
                          ├─▶ TelemetryHook       events + the run's bill (gen_ai.*)
                          │                       inside the loop's own spans
                          ├─▶ AuditHook           who asked, what was answered,
                          │                       which data was touched
                          ├─▶ SessionRecorder     every event as JSONL → replay
                          ├─▶ BroadcastHook       live feed for UIs and monitors
                          ├─▶ TranscriptRecorder  turns into any Memory, with a TTL
                          └─▶ your #[hook]        policy, approval gates, …
```

## Crates

The workspace publishes as `harness-rs-*`. Upper layers build on lower ones; the
middle row are siblings you pick à la carte. A crate boundary exists only where
it lets you skip a dependency tree you might not want — heavy backends (SurrealDB,
SQLite, gRPC, OTLP) sit behind features.

```text
┌──────────────────────────────────────────────────────────────────────────────────┐
│ harness-cli · harness-serve      the `harness` binary · sessions, auth, audit    │
│                                  · http (axum) · grpc (tonic) · /metrics         │
├──────────────────────────────────────────────────────────────────────────────────┤
│ harness-loop                     ReAct loop · guards · acceptance · compaction   │
│                                  · hooks · traces + metrics · replay · sandbox   │
│                                  · subagents · scheduler · orchestrator          │
│                                  · loop engine (L1 → L2 → L3)                    │
├────────────────────┬──────────────────────────────────┬──────────────────────────┤
│ harness-models     │ harness-tools                    │ harness-memory           │
│ OpenAI-compatible  │ harness-tools-web                │ harness-cortexdb         │
│ Anthropic          │ harness-tools-docs               │                          │
│ Gemini native      │ harness-tools-sql                │ backend picked by URL    │
│ MockModel          │ harness-mcp-client               │                          │
├────────────────────┴──────────────────────────────────┴──────────────────────────┤
│ harness-core · harness-context  traits, lifecycle Event, default World runtime   │
│ harness-macros                  #[tool] #[skill] #[guide] #[sensor] #[hook]      │
└──────────────────────────────────────────────────────────────────────────────────┘
```

| Crate | What it is |
|---|---|
| `harness-rs-core` | The traits and types: `Model`, `Tool`, `Guide`, `Sensor`, `Hook`, `Compactor`, `Skill`, `Context`, `World`, `Event` |
| `harness-rs-context` | Default `World` runtime (Tokio process runner, system clock, in-memory KV) |
| `harness-rs-loop` | The agent loop and everything that runs with it, including its spans and metrics; feature `otel` adds OTLP export |
| `harness-rs-models` | `ApiKind::build(base_url, model, key)` for OpenAI-compatible, Anthropic and Gemini native; embeddings, image, speech; `MockModel` |
| `harness-rs-tools` | Files, shell (with background jobs), memory, skills, tasks, dates, browser, recall, sensors, MCP server |
| `harness-rs-tools-web` | `GroundedWebSearch` (the model's own search first, scraper fallback) and URL fetch |
| `harness-rs-tools-docs` | `read_document`: PDF/Word/Excel/PowerPoint parsed locally, LLM fallback |
| `harness-rs-tools-sql` | SELECT-only SQL with auto-LIMIT and result redaction |
| `harness-rs-mcp-client` | A remote MCP server's tools as harness tools |
| `harness-rs-memory` | Every memory/recall backend behind a URL: JSONL, `sqlite://`, `mem://` / `surrealkv://` / `ws://` (SurrealDB), `cortexdb+http://` / `cortexdb://` — each behind a feature |
| `harness-rs-cortexdb` | `Memory` over a CortexDB brain |
| `harness-rs-serve` | Multi-session serving core; features `http` (axum), `cors`, `grpc`, `prometheus` (`GET /metrics`), `otel` |
| `harness-rs-cli` | The `harness` binary; feature `otel` exports runs when `OTEL_EXPORTER_OTLP_ENDPOINT` is set |
| `harness-rs` | Facade: `use harness::prelude::*` |

## Quick start

```rust
use harness_core::Task;
use harness_loop::AgentLoop;
use harness_models::ApiKind;
use harness_tools::fs::{ListDir, ReadFile};
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

The same builder takes the rest of the harness: `with_guide`, `with_sensor`,
`with_hook`, `with_acceptance`, `with_compact_policy`, `with_stuck_policy`,
`with_tool_timeout`, `with_streaming`, `with_cancellation`, `with_recall`.
`session()` gives a multi-turn `Session` that keeps a stable prompt prefix so
provider caches hit across turns.

Register tools, skills, guides, sensors and hooks with `#[harness::tool]` /
`#[skill]` / `#[guide]` / `#[sensor]` / `#[hook]` — they auto-register via
`inventory`. Scaffold a project with `harness new`; `harness code` is an
approval-gated coding REPL built on the framework (`--yolo` to unattend).

## Observability

**Traces.** Every run is an `agent_run` span with one child span per model call
(`chat {model}`) and per tool call (`execute_tool {tool}`), opened by the loop
itself, so parallel tool calls show up as overlapping bars and a failed call is
marked as an error. Fields follow the OpenTelemetry GenAI conventions
(`gen_ai.usage.input_tokens`, `gen_ai.tool.name`, …). Enable the `otel` feature
and call `harness_loop::otel::init_tracing_with_otlp` (or add
`otel::otlp_layer` to your own subscriber) to export to Jaeger, Tempo, SigNoz,
Logfire or Langfuse. The CLI does the same when built with `--features otel`
and `OTEL_EXPORTER_OTLP_ENDPOINT` is set.

```text
agent_run ─────────────────────────────────────────────────  outcome=done
  chat deepseek-chat ────────                                in=1200 out=80
  execute_tool read_file       ─────                         ok
  execute_tool grep            ───────                       ok   (parallel)
  execute_tool shell_exec             ──── ✗                 error
  chat deepseek-chat                       ──────────────    in=2100 out=310
```

**Events and totals.** `TelemetryHook` adds per-call events and one `run.end`
line with the bill: tokens, model/tool calls, failures, compaction savings, time
to first token (target `harness.telemetry`; `harness run --telemetry`).

**Metrics.** The loop records through the `metrics` facade: model-call duration
and token usage (`gen_ai.client.operation.duration`, `gen_ai.client.token.usage`),
tool-call duration by tool and success, run duration by outcome, and tokens
saved by compaction. See `harness_loop::meters`. With no recorder installed
this is free. `harness-rs-serve`'s `prometheus` feature installs one and serves
`GET /metrics`.

**Serving.** `ChatService` attaches a `TelemetryHook` to every request and runs
it in a `chat_request` span carrying the request id, which is the same id the
audit trail and the replay recording use.

**Records.** `SessionRecorder` writes the run as JSONL, which `harness trace`
pretty-prints, `harness trace --html out.html` turns into a self-contained
timeline page (parallel calls overlapping, a click for each call's details), and
`harness replay` reruns offline with no LLM calls.
`AuditHook` keeps a compliance trail (optionally redacted and hash-chained).
`BroadcastHook` streams the lifecycle live to any number of subscribers.

## More

**[docs/guide.md](docs/guide.md)** — crates, composable layers, sandboxing, PII
redaction, documents/OCR, telemetry, record/replay, grounded search, examples ·
**[examples/](examples)** — runnable agents, from `deepseek-hello` to
`ecommerce-ops-agent` · **[docs/benchmarks.md](docs/benchmarks.md)** — `pass^k`,
guard ablation, cost · **[DESIGN.md](DESIGN.md)** · **[CHANGELOG.md](CHANGELOG.md)**
· [MIT](LICENSE)
