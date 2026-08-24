# Guide

What the crates give you, and how they compose. Rationale lives in
[DESIGN.md](../DESIGN.md); numbers live in [benchmarks.md](benchmarks.md).

## What you get

| Layer | What | Crate |
|---|---|---|
| **Models** | 3 protocol families (OpenAI-compat · Anthropic · Gemini native), one `ApiKind::build(url, model, key)` · Gemini search grounding on by default | `harness-models` |
| **Tools** | fs · shell (risk-gated) · web search/fetch (`GroundedWebSearch`: the model's own search first, scraper fallback) | `harness-tools-*` |
| **Loop** | ReAct + tool dispatch + sensor feedback + auto-fix · guards: stuck detection, acceptance checks, result ceiling with disk **spill** (nothing lost), per-call tool deadlines | `harness-loop` |
| **Loop engineering** | recurring loops: maturity levels L1/L2/L3, human gates, action executors, token budgets | `harness-loop-engine` |
| **Orchestration** | async Run = concurrent Job DAG + retry/backoff + dynamic replanning + resumable state | `harness-orchestrator` |
| **Learning** | record episodes (situation → tools used → outcome) + semantic recall · CortexDB-backed `Memory` | `harness-experience`, `harness-cortexdb` |
| **Skills · Guides · Hooks · Sensors** | proc-macro registered, agentskills.io-compliant | `harness-macros`, `harness-skills` |
| **Memory · Recall** | `Memory` trait + JSONL store · cross-session search (FTS5 / CJK) | `harness-core`, `harness-recall-sqlite` |
| **Privacy** | PII detect + redact (label/mask/hash/block, Luhn-checked cards) · redact-on-write `Memory` decorators | `harness-redact`, `harness-context` |
| **Documents** | `read_document` — PDF/Word/Excel/PPT locally (pure Rust) · offline OCR for scanned PDFs (`pdftoppm` + `tesseract`) · LLM/vision fallback | `harness-tools-docs` |
| **Observability** | `TelemetryHook` (structured `tracing` spans → OTLP) · JSONL session record + deterministic offline `replay` | `harness-loop` |
| **Scheduler · MCP · Sandbox · CLI** | recurring jobs · MCP server+client · OS-native sandbox (macOS Seatbelt) · git-worktree / Docker · `harness code` / `run` / `replay` / `trace` / `sched` / `new` / `mcp serve` | — |

## Composable layers

Start with one agent; compose upward as the work grows.

- **`harness-loop`** runs *one* agent (ReAct: think → call tools → observe).
- **`harness-loop-engine`** governs a *recurring* loop: it earns autonomy in
  stages — **L1 report** → **L2 assisted** (human gates every change) → **L3
  unattended** (allowlisted actions only) — under a token budget, with an
  `ActionExecutor` for the side effect after a verified approval.
- **`harness-orchestrator`** fans *one goal* across many concurrent, dependent
  Jobs (a DAG) with retry/backoff, a run budget, crash-resumable state, and
  **dynamic replanning** (a `Planner` mutates the DAG mid-run from results).
- **`harness-experience`** makes an agent *learn*: it records each run as an
  episode (situation → tools used → outcome) and recalls similar past episodes
  to guide the next run. Pair with **`harness-cortexdb`** (a CortexDB-backed
  `Memory`) for semantic recall over a brain shared with Claude Code / Codex.

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

## Design principles

- **Don't burn tokens on what code can do** — lint/format/git run via Sensors
  and Hooks, not the model. The Compactor manages scarce context.
- **Isolate, don't interrupt** — permissions are decided at sandbox spawn, not
  prompted per call. Backends are honest about what they *enforce*
  (`Isolation::{None, Changes, Process}`): `SeatbeltSandbox` (macOS, kernel-level
  via `sandbox-exec`) and `ContainerSandbox` (Docker) enforce; `WorktreeSandbox`
  isolates git *changes*, not capability. Today a sandbox wraps shell exec;
  in-process fs tools are jailed separately.
- **Earn autonomy in stages** — start at L1, set a budget, graduate only as you
  build trust. Unattended loops make unattended mistakes; verification is on you.

## Coding agent

`harness code` is an interactive, opencode-style coding REPL built entirely on
the framework above — multi-turn, streaming, with read/write/edit/list/grep/glob
and shell tools. It runs in **NORMAL** mode (every write, edit, and shell command
waits for a `y/N` you approve) or **`--yolo`** (unattended). A single Rust binary,
any OpenAI-compatible model:

```sh
HARNESS_API_KEY=… HARNESS_BASE_URL=… HARNESS_MODEL=… harness code            # NORMAL
harness code --yolo --workspace .                                            # YOLO
```

## Privacy & documents

**Redact PII before it persists.** `harness-redact` detects card numbers
(Luhn-checked, so order numbers survive), emails, phones, and money, then
rewrites them — `Label` (`<EMAIL>`), `Mask` (`************1111`), `Hash` (a
stable pseudonym), or `Block`. It's *redact-not-drop*: the surrounding fact is
kept. Two `Memory` decorators wire it to the two write boundaries that leak:

```rust
use harness_context::{GuardedMemory, RedactingMemory};

// (1) agent long-term memory — redact on write; money/secrets drop
let memory: Arc<dyn Memory> = Arc::new(
    GuardedMemory::new(file_mem).with_blocked_substring("password"),
);

// (2) transcript / experience → CortexDB — redact-only, never drops a turn
let safe: Arc<dyn Memory> = Arc::new(RedactingMemory::new(cortex_mem));
spawn_transcript_writer(rx, safe);   // every captured turn is scrubbed
```

Or use the engine directly on any text: `Redactor::new().scrub(text).text`.

**Read documents, including scanned PDFs.** `read_document` extracts text from
PDF/Word/Excel/PowerPoint locally in pure Rust (zero tokens). A scanned,
image-only PDF has no text layer — enable the `ocr-tesseract` feature for
offline OCR (rasterise with `pdftoppm`, recognise with `tesseract`; still zero
tokens, still deterministic):

```toml
harness-rs-tools-docs = { version = "0.0.44", features = ["ocr-tesseract"] }
```

```rust
let tool = ReadDocument::new();                    // local + OCR
let tool = ReadDocument::with_llm_fallback(model); // + vision fallback for images
// model call: {"path": "scan.pdf", "ocr_lang": "eng+chi_sim"}
// result carries source = "local" | "ocr" | "llm"
```

## Observability

Two seams, one instrumentation. **`TelemetryHook`** projects the agent's
lifecycle onto structured `tracing` spans (`agent_run` → `model.complete` /
`tool.call` / `budget.warning`, with token, latency, and ok/err fields).
Attach `tracing-opentelemetry` and they export to Jaeger / Tempo / any OTLP
backend unchanged; attach `tracing_subscriber::fmt().json()` for a log pipeline.

**Record + replay** makes a run reproducible for free:

```sh
harness run "…" --workspace ./ws --write --record run.jsonl   # capture live
harness replay run.jsonl --workspace ./ws                      # re-run offline, no LLM
harness trace  run.jsonl --verbose                             # inspect the timeline
```

`replay` drives the loop from the recorded model outputs (a `MockModel`) and
re-executes the tool calls, so it reproduces the exact Outcome with zero API
cost — record once, regression-test in CI forever.

## Web search that uses the model's own engine

Providers with server-side grounding (Gemini's googleSearch) search better than
an HTML scraper: fresher index, no bot-walls, an answer with sources instead of
links to fetch. `GroundedWebSearch` registers under the same `web_search` name,
asks `Model::search_web` first, and falls back to the DuckDuckGo/Bing scraper
when the model has no grounding — registering it is never a downgrade:

```rust
let model: Arc<dyn Model> = Arc::new(OpenAiCompat::with_key(base, "gemini-3.6-flash", key));
AgentLoop::boxed(model.clone())
    .with_tool(Arc::new(GroundedWebSearch::new(model)))
```

On the native Gemini provider grounding is on by default in the main channel
(the model can search mid-loop); `.with_search_grounding(false)` turns it off.

## Examples

See **[examples/](../examples/)** — memory, recall, the scheduler, MCP,
**`redaction-demo`** (PII redaction: the engine + `GuardedMemory` /
`RedactingMemory`), **`experience-cortexdb`** (the learning layer over a CortexDB
brain), **`cap`** (a coding agent reimplementing
[oh-my-pi](https://github.com/can1357/oh-my-pi)'s **hashline editing** —
content-hash line anchors instead of line numbers), and two end-to-end agents
over a live PostgreSQL database: **`ecommerce-analyst`** (concurrent analysis
DAG) and **`ecommerce-ops-agent`** (the full stack — dynamic replanning,
L1/L2/L3 governed DB writes, cross-run memory).
