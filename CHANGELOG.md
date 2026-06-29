# Changelog

All notable changes to the **harness-rs** workspace. Versioning is shared across
every `harness-rs-*` crate (workspace-level `[package].version`).

## 0.0.18

### Fixed

- **`harness-rs-orchestrator` — `RunReport::render` no longer panics on
  multibyte text.** The summary truncation sliced the job result at a fixed
  byte index (`&t[..80]`), which panics when byte 80 falls inside a multi-byte
  UTF-8 char (e.g. an emoji or CJK character in a model's output). It now
  truncates on a character boundary. Regression test added.

## 0.0.17

New **orchestration** layer: run one goal as a concurrent DAG of Jobs.

### Added

- **`harness-rs-orchestrator` — single-machine async Run orchestration.** A new
  crate that fans one goal out across many concurrent, dependent Jobs — the
  durable task fabric of an agent system, kept deliberately single-machine (no
  Kafka, no worker pool, no distributed locks; just `tokio` + a state store):
  - **Concurrent Job DAG** — a `Dag` of `Job`s; the `Orchestrator` runs every
    Job whose dependencies have `Succeeded`, up to a concurrency cap, on one
    thread via `FuturesUnordered` (sub-agent futures are `!Send`). Each Job
    gets a fresh `World` from a factory for worker-style isolation.
  - **Dynamic replanning** — a `Planner` is re-invoked with the results so far
    and may merge new Jobs into the running DAG (`PlanDelta::Add`), the
    feedback edge a static plan-then-execute workflow lacks.
  - **Retry / backoff / dead-letter** — per-Job `RetryPolicy` with
    `Backoff::{None, Fixed, Exponential}`; exhausted Jobs are `DeadLettered`
    and their dependents `Cancelled`.
  - **Resumable state** — a `RunStore` (`InMemoryRunStore` / `FileRunStore`)
    persists Run + Job state after every transition; `Orchestrator::resume`
    restarts a crashed Run from its succeeded results.
  - **Run-level token budget** — `RunBudget` caps total spend across all Jobs.
  - Up-front DAG **cycle rejection**. Execution is decoupled via the
    `JobRunner` trait; the default `SubagentJobRunner` runs each Job as an
    isolated sub-agent. See DESIGN.md §11.6 and `examples/orchestrator-demo`.

## 0.0.16

New **loop-engineering** layer, plus a simplified, hardcoded-URL-free model API.

### Added

- **`harness-rs-loop-engine` — loop engineering for harness-rs.** A new crate
  that turns the existing building blocks (scheduler, sandbox, sub-agents,
  memory, MCP) into *trusted recurring loops*. It adds the orchestration
  discipline those parts lacked:
  - **`LoopLevel`** — maturity levels `L1Report` → `L2Assisted` → `L3Unattended`
    (a loop earns autonomy in stages; the level governs both write-capability
    and gate policy).
  - **`HumanGate`** — proceed-or-escalate decisions tied to the level
    (`AlwaysEscalate`, `AllowlistGate`, `CallbackGate`).
  - **`TokenBudget` / `BudgetState`** — per-round input/output/total token
    ceilings, tallied across the maker and checker sub-agents.
  - **`LoopSpec`** — an inert, serializable loop definition; its required
    `intent` field is the antidote to *intent debt*.
  - **`LoopEngine::run_once`** — one verified, budgeted, gated round: recall
    state → isolate → **maker** sub-agent → **checker** sub-agent → gate →
    record state. Never panics or returns `Err` (failures fold into
    `RoundOutcome::Failed`).
  - **`LoopScheduler`** — runs loops on their declared cadence.
  - **`patterns`** — seven ready-made production loops: `daily_triage`,
    `pr_babysitter`, `ci_sweeper`, `dependency_sweeper`, `changelog_drafter`,
    `post_merge_cleanup`, `issue_triage`.

  See DESIGN.md §11.5. Example: `examples/loop-engine-demo`.
- **`harness-rs-models` — `ApiKind` single entry point.** `ApiKind::{OpenAI,
  Anthropic, Gemini}.build(base_url, model, key) -> Arc<dyn Model>` constructs
  any of the three protocol families through one call.

### Changed

- **`harness-rs-models` — no more hardcoded provider URLs (breaking).** The
  `providers` module and its vendor URL menu (`DEEPSEEK`, `OPENAI`, `GROQ`,
  `TOGETHER`, `OLLAMA`, `ANTHROPIC`, `GEMINI`) are **removed**. There are exactly
  three protocol families and you always pass `base_url` yourself.
  `AnthropicNative::with_key` and `GeminiNative::with_key` now take
  `(base_url, model, key)` to match `OpenAiCompat::with_key` — no URL is baked
  into any adapter. Migration: replace `providers::DEEPSEEK` with the literal
  `"https://api.deepseek.com"`, etc.
- **`harness-rs-loop` — `SubagentReport` now carries `usage`.** The
  `harness_core::Usage` from the sub-agent's loop is preserved on the report
  (previously discarded), so callers can account for token spend across
  sub-agent turns. `BudgetExhausted` rounds also surface their `last_text`.
- **`harness-rs-loop-engine` — L1 now hard-filters mutating tools.** Report-only
  loops no longer rely only on prompt text for read-only behaviour: L1 maker and
  checker sub-agents receive only `ReadOnly` / `Network` tools. `Idempotent` and
  `Destructive` tools are skipped with a trace log.
- **`harness-rs-loop-engine` — action executors for approved work.**
  `LoopEngine` now has an `ActionExecutor` handoff. When a verified proposal is
  auto-approved, the executor is invoked and its `ActionReceipt` is attached to
  the `RoundReport`; executor failures become `RoundOutcome::Failed` instead of
  pretending the action landed. The safe default is `ApprovalOnlyExecutor`, and
  apps can install `CallbackActionExecutor` or their own async executor via
  `with_action_executor`.
- **`harness-rs-sandbox` — VM isolation is now explicitly deployment-owned.**
  The non-functional `VmSandbox` / Firecracker stub has been removed from the
  core crate. VM or microVM isolation should be provided by downstream
  infrastructure crates that implement the existing `Sandbox` trait.

### Tests

- Added deterministic `LoopEngine::run_once` coverage for L1 tool filtering, L3
  allowlisted auto-proceed, budget exhaustion before checker execution, and
  memory recall/writeback of the loop state spine. Added action-executor
  coverage for successful handoff and failed handoff.

## 0.0.14

Skill loading is now interop-friendly and fault-isolated. Additive, backward-compatible.

### Fixed

- **`harness-rs-skills` — one bad skill no longer hides them all.**
  `scan_skills_root` previously did `load(&p)?`, so a single malformed
  `SKILL.md` aborted the entire scan and the agent saw *zero* skills. It now
  **skips the offending skill with a `tracing::warn!`** and returns every valid
  one. Regression test `scan_skips_invalid_skill_keeps_the_rest`.

### Changed

- **`harness-rs-skills` — tolerate non-spec frontmatter fields.** Skills from
  the wider ecosystem (skills.sh, Claude Code, …) routinely carry extensions
  like `displayName` / `hidden`. The loader used to **reject** any unknown
  top-level field; it now **logs and ignores** them (the field is dropped on
  deserialize), so those skills load instead of failing. Spec guidance is
  unchanged — extensions still belong under `metadata`. Test
  `rejects_unknown_top_field` → `tolerates_unknown_top_field`.

## 0.0.13

`forget_memory` can now delete in a single tool round. Additive, backward-compatible.

### Added

- **`harness-rs-tools-memory` — one-call `forget_memory`.** `ForgetMemoryTool`
  gains `with_resolver(Arc<dyn Memory>)`: when wired, the tool accepts a natural
  language `query` (the fact in the user's own words) in addition to an exact
  `id`, recalls the single best match, and deletes it. This collapses the usual
  `list_memories` → `forget_memory` two-round dance into one call, cutting an LLM
  round-trip off every delete. Without a resolver the tool keeps its prior
  id-only behaviour; if both `id` and `query` are given, `id` wins. Added
  regression tests covering query resolution, id precedence, a no-match miss, and
  the id-only rejection path.

## 0.0.12

Security fix for the skill-management tool. Additive (no breaking changes).

### Security

- **`harness-rs-tools-skills` — `skill_manage` path-traversal hardening.** The
  `patch` action joined the user-supplied skill `name` into a path and read the
  `SKILL.md` *before* validating the name, so a crafted name like
  `../other/skill` could read a file outside the tool's skills dir (a low-severity
  existence-probe leak in multi-tenant hosts — no write, no content exfiltration).
  `validate_name` now runs up front in `SkillManageTool::invoke` for **every**
  action before any filesystem access. Added a `patch_rejects_traversal_name`
  regression test.

## 0.0.11

Security fix for the MCP HTTP client. Additive (no breaking changes).

### Security

- **`harness-rs-mcp-client` — SSRF-safe HTTP connect.** `connect_http` uses a
  default reqwest client that follows redirects and re-resolves DNS, so a
  validated URL can still be redirected (`302 → http://169.254.169.254/…`) or
  DNS-rebound to an internal target. New **`McpClient::connect_http_with_client(url,
  client)`** lets the caller pass a hardened `reqwest::Client`
  (`redirect::Policy::none()` + `.resolve(host, vetted_ip)`), closing the
  redirect-bypass and DNS-rebinding holes while keeping the security policy on the
  caller's side. The matching `reqwest` is re-exported as
  `harness_mcp_client::reqwest` so client types unify. `connect_http` now carries
  an explicit SSRF warning in its docs.

## 0.0.10

100% MCP client transport coverage. Pure addition on top of 0.0.9.

### Added

- **`harness-rs-mcp-client` — Streamable HTTP transport.** New
  `McpClient::connect_http(url)` connects to a remote MCP server over Streamable
  HTTP (the standard remote MCP transport; SSE is subsumed by it), in addition to
  the existing `connect_stdio` child-process transport. Behind the `http` feature
  (on by default; `default-features = false` drops the reqwest dependency). The
  tool-proxy layer is transport-agnostic, so remote-tool results flow back through
  the agent loop exactly as with stdio.

## 0.0.9

Thinking-model + local-tool-calling fixes for the OpenAI-compat adapter,
shaken out against Qwen3 on Ollama. Backward-compatible.

### Fixed

- **No-arg tool calls no longer 400 on strict backends.** A tool call with no
  arguments was echoed back with `arguments: ""`, which Ollama rejects
  (`HTTP 400 invalid tool call arguments`). `OpenAiCompat` now normalises any
  non-object arguments to `"{}"` when serializing the assistant turn.
- **Thinking-model replies no longer come back blank.** Models that emit the
  whole answer into the reasoning channel and leave `content` empty (e.g. Qwen3
  via Ollama) now surface that reasoning as the turn's text — both in
  `OpenAiCompat::complete` and in the streaming agent loop when a turn ends with
  no text, no tool calls, and non-empty reasoning.
- **Streamed reasoning is concatenated verbatim** instead of being joined with
  newlines, so fallback replies read as prose rather than one word per line.

### Added

- **`OpenAiCompat` now captures Ollama's `reasoning` field** (in addition to
  DeepSeek's `reasoning_content`) on both the non-streaming and streaming paths.
- **`HARNESS_OPENAI_EXTRA_BODY`** — a JSON object merged into every
  chat-completions request body. Lets callers pass provider-specific knobs the
  typed request doesn't model, e.g. disable Qwen3 thinking on Ollama with
  `{"chat_template_kwargs":{"enable_thinking":false}}`.

## 0.0.8

Local-model ergonomics — an Ollama embeddings adapter and a configurable HTTP
timeout. Pure addition on top of 0.0.7.

### Added — Ollama embeddings

- **`OllamaEmbed`** (`harness-rs-models`) — implements `harness_core::Embedder`
  against a local Ollama server's OpenAI-compatible `/v1/embeddings` endpoint.
  Defaults to Google's `embeddinggemma` (768-dim); `OllamaEmbed::with_model`
  overrides the model/dim. Pairs with `OpenAiCompat::with_key(providers::OLLAMA,
  ..)` for a fully-offline chat + vector-search stack. Opt-in: the chat adapters
  do not reference it.

### Changed — OpenAI-compat timeout

- `OpenAiCompat`'s per-request HTTP timeout (previously a hardcoded 120s) is now
  configurable via `HARNESS_HTTP_TIMEOUT_SECS`, for slow local backends whose
  first-token latency on large models can exceed two minutes. Default unchanged
  at 120s.

## 0.0.7

MCP client — consume external MCP servers from an `AgentLoop`. Pure addition
on top of 0.0.6.

### Added — MCP client

- **New crate `harness-rs-mcp-client`** — a generic MCP (Model Context Protocol)
  client built on the official `rmcp` 1.7 SDK. `McpClient::connect_stdio(program,
  args)` spawns an MCP server as a child process over stdio, lists its tools, and
  exposes each as a harness `Arc<dyn Tool>` (`.tools()` /
  `.tools_with_read_only(names)` / `.tool_names()`). MCP results flow back through
  the standard `AgentLoop` path (`PreToolUse` / `PostToolUse`, session record,
  context) — not a side channel. Complements `harness-rs-mcp` (the server side).
  Verified end-to-end against CortexDB's MCP server (47 RAG/GraphRAG tools;
  `knowledge_save` → `knowledge_search` round-trip).
- Remote tools default to `Destructive` risk; `tools_with_read_only` marks named
  tools `ReadOnly`. Non-object tool args are rejected with `InvalidArgs`; non-text
  content blocks (image/resource/audio) are surfaced via `tracing::warn!` + an
  `omitted_content` key instead of being silently dropped.

### Added — CI

- CI runs the `harness-rs-mcp-client` integration tests and clippy under its
  `test-server` feature (which gates a test-only echo MCP stdio server).

## 0.0.6

FileRecall robustness + release automation. No breaking source changes.

### Fixed

- **`harness-context` FileRecall** — filename sanitization now caps by **bytes**
  (not chars) and hashes over-long names, fixing `ENAMETOOLONG` on Linux for
  long / non-ASCII session keys.

### Added — release

- **Release workflow** — pushing a `v*` tag verifies the tag matches the
  workspace version, runs the test gate, then publishes every `harness-rs-*`
  crate to crates.io in dependency order via `cargo ws publish`.
- README tour sections for recall / learning-loop / scheduler.

## 0.0.5

Three capabilities — cross-session **recall**, a self-evolving **learning loop**,
and in-process **scheduling** — plus new crates. Additive on top of 0.0.4.

### Added — recall (cross-session search)

- **`RecallStore` trait** with two backends: `harness_context::FileRecall` (JSONL)
  and the new optional crate **`harness-rs-recall-sqlite`** (SQLite FTS5 + trigram
  tokenizer for CJK, BM25 ranking). `AgentLoop::with_recall` / `.auto_inject`, an
  owner-scoped `SessionSearchTool` (three query shapes), and an opt-in
  `RecallGuide`. A shared contract test suite covers both backends including
  owner isolation.

### Added — learning loop (self-evolving skills + memory)

- **`AgentLoop::with_learning_loop(LearningConfig)`** — forks a review subagent at
  `SessionEnd` (threshold-gated, best-effort) that patches skills/memory from the
  transcript. New crate **`harness-rs-tools-skills`** with the `skill_manage` tool
  (create/edit/patch/delete `SKILL.md`); `harness-skills` gains `write_skill_md` /
  `delete_skill` (validate-on-write).

### Added — scheduling

- **New crate `harness-rs-scheduler`** — `Job` + `JobStore` / `FileJobStore`, a
  `Scheduler` that ticks and runs a job as a subagent turn, a `Channel` trait with
  `StdoutChannel` + `EmailChannel` (Resend), and a `cronjob` tool for agent
  self-scheduling (schedule-string validated).

### Changed

- **`harness-core`** — `Arc<dyn Model>` is now used via the `DynModel` newtype
  (replacing the blanket `impl Model for Arc<dyn Model>`, which overflowed the
  `Send` auto-trait solver in some async contexts).

## 0.0.4

Observability and open long-term memory. No breaking source changes; pure
additions on top of 0.0.3.

### Added — observability

- **`harness_loop::LiveProgressHook`** — `Hook` that streams every model call,
  tool call, and tool result to stderr in real time. Pair with
  `AgentLoop::with_hook` to watch what the agent is doing instead of staring
  at a silent terminal. Independent of `SessionRecorder`; both can be
  installed together.
- **`harness_loop::format_event_verbose`** — multi-line formatter that surfaces
  model text, reasoning, full tool args, tool result preview, and failure
  reasons (errors / hint / message / error keys). Used by the live hook and
  by `harness trace --verbose`.
- **`harness trace --verbose`** (alias `-v`) — selects the verbose formatter
  when pretty-printing a recorded JSONL session.
- **`Event::BudgetWarning { ratio }`** is now fired (was defined but unused).
  Currently emitted exactly once, with `ratio = 1.0`, immediately before the
  forced final-synthesis pass — so observers can clearly label that boundary.
  `SessionEvent::BudgetWarning` mirrors it for replay.

### Added — loop completeness

- **Forced final-synthesis on budget exhaustion** — when `run_with_max_iters`
  would otherwise return `Outcome::BudgetExhausted { last_text: None, .. }`,
  the loop makes one extra tool-less model call asking for the best-effort
  conclusion. The result lands in `last_text`. Closes the "agent burned all
  iterations on tool calls, returned no answer" failure mode. Regression
  test: `budget_exhausted_forces_final_synthesis_into_last_text`.

### Added — long-term, open memory

The piece Harrison Chase ("your harness, your memory") and Viv Trivedi
("distil traces into higher-level memory primitives") call out as the
moat against provider lock-in. All on the user's disk; nothing on a
third-party server.

- **`harness_core::Memory`** trait + **`MemoryEntry`** + `MemoryError`.
- **`harness_context::FileMemory`** — append-only JSONL backend with
  keyword-overlap recall (ties broken by recency). No embedding deps;
  swap-in a vector backend by implementing the trait.
- **`harness_loop::MemoryGuide`** — Guide::Always; at session start calls
  `recall(task.description, top_k)` and injects the hits into `ctx.guides`
  as a single `Block::Text` so the model sees them in the system prompt.
- **`harness_loop::MemoryWriter`** — Hook that persists the verbatim final
  assistant text on `TaskCompleted` (skips `BudgetExhausted`).
- **`harness_loop::MemorySynthesizer`** — smarter alternative: uses a cheap
  separate "synth model" (e.g. `deepseek-v4-flash`, `gpt-5-nano`) to
  distil each session into 1-3 atomic durable facts tagged for retrieval.
  Markdown fences tolerated; unparseable model output falls back to a
  `"synth-raw"` entry rather than silent drop. `flush_pending()` awaits
  spawned writes so callers can guarantee persistence before `main()`
  returns (otherwise tokio runtime drop cancels in-flight commits).

### Examples

- `--progress` / `HARNESS_PROGRESS=1` installs `LiveProgressHook` on
  `personal-assistant` and `investor-bot`.
- `--record <path>` writes a JSONL session log (parity between both
  examples).
- `--memory <path>` + `--synth-model <id>` (env: `HARNESS_SYNTH_MODEL`)
  installs `MemoryGuide` + `MemorySynthesizer` on both examples. Synth
  model defaults to `deepseek-v4-flash`.
- `HARNESS_BASE_URL` / `HARNESS_MODEL` / `HARNESS_API_KEY` env vars let
  the same binaries drive any OpenAI-compatible endpoint without code
  edits; DeepSeek defaults preserved.
- Both `BudgetExhausted` print sites now surface `last_text` (the
  forced-synthesis answer).
- investor-bot SYSTEM_PROMPT strengthened with explicit budget rules:
  stop retrying after 2 empty searches; abandon URLs returning
  401/403/503; commit to a partial answer marking unverified facts as
  UNKNOWN.

### Fixed

- `harness new` scaffold was pinning `0.0.1` deps (pre-publish; would
  never build) and using the wrong package names in `[patch.crates-io]`
  (`harness = ...` instead of `harness-rs = ...`, so `--local` never
  actually patched). Now pins `0.0.4`, correct published names, and a
  `main.rs` that demonstrates the env-var endpoint config and
  `HARNESS_PROGRESS` opt-in.

### Tests

- 133 passing (was 123). 10 new tests cover live progress, forced
  synthesis, budget-warning emission, file memory round-trips,
  memory-writer persistence, memory-synth JSON parsing + fence stripping
  + raw-fallback, and the cross-session end-to-end recall.

## 0.0.3

Re-publish of the 0.0.2 feature set so that every workspace crate ships
consistent code. 0.0.2 went out in stages, and several crates landed
on crates.io before the `PreAutoFix`/`PostAutoFix` events were added
to `harness-rs-core`. Downstream consumers that bumped a single crate
to 0.0.2 could hit `error[E0599]: no variant named PreAutoFix`. 0.0.3
fixes that by re-publishing every crate at the same source revision.
No new features — depend on 0.0.3 over 0.0.2 if you want any of the
"0.0.2" CHANGELOG entries below to actually be present.

## 0.0.2

The first version anyone outside this checkout should depend on. Adds a
proper user-profile mechanism, daemon scheduler, retry/backoff in the model
adapters, and closes the security holes from the self-audit.

**Known issue:** published in stages; crates published before
`PreAutoFix`/`PostAutoFix` were added to `harness-rs-core` are missing
those events. Use 0.0.3 instead.

### Added

- **`harness-rs-daemon`** — optional standalone scheduler crate. Reads a
  declarative TOML config (`daily HH:MM` / `weekly mon HH:MM` / `every Nm`)
  and spawns each job as a subprocess. Pair with launchd / systemd to run
  forever. Does not depend on any other `harness-rs-*` crate.
- **`UserProfile` + `ProfileGuide`** in core/loop — ambient user context
  (name, timezone, locale, free-form `extra` map) that any tool can read
  from `World.profile`. The framework provides the slot; apps decide where
  the data comes from. Opt-in `ProfileGuide` injects it into the system
  prompt.
- **`AgentLoop::run_with_seed_history`** — lets REPL apps push prior
  conversation into `ctx.history` (where the compactor sees it) instead of
  string-concatenating into `task.description` (where it didn't).
- **Retry/backoff** in `OpenAiCompat::complete` and `AnthropicNative::complete`.
  5xx + 429 + send/body errors retried up to 3× with 1s/2s/4s backoff.
  Other 4xx (auth, bad request) propagate immediately.
- **`Outcome` partial-work surface** — `Outcome::Done` and
  `Outcome::BudgetExhausted` now both carry `tools_called: u32`,
  `usage: Usage`, and (for BudgetExhausted) `last_text: Option<String>`.
  Both variants `#[non_exhaustive]`.
- **MCP server resources + prompts** — `harness-rs-mcp` gains
  `resources/list`, `resources/read`, `prompts/list`. Skills can be mounted
  via `McpServer::with_skill(...)` and become `harness://skill/<name>`
  resources for any MCP client (Claude Code / Cursor / Codex).
- **`harness mcp serve --skills <dir>`** — CLI flag to expose a filesystem
  skills directory as MCP resources without writing code.
- **`Event::PreAutoFix` + `Event::PostAutoFix`** — hooks can intercept
  sensor-emitted `FixPatch` patches per-patch. Default safelist denies
  `RunCommand` outside `cargo fmt|clippy|fix / rustfmt / gofmt / prettier
  / ruff / black`. Hooks may widen with `HookOutcome::Allow`.
- **`is_default_safe_fix(&FixPatch) -> bool`** — public, so apps can run
  the same gate independently.
- **Wider `shell_read` allowlist** — `npm/pnpm/yarn/bun`, `python/pip` family
  (read-only only), `node/deno --version`, `go` (version/env/list/vet/doc/fmt),
  `make --version|--dry-run`, `docker/podman/kubectl` inspection subcommands,
  plus `tree / stat / file / du / df / ps / uname / hostname / date / env /
  which / whereis`. Write subcommands explicitly rejected with a 'use
  shell_exec' message.
- **Multi-engine search** in `examples/investor-bot`: DuckDuckGo → Bing
  fallback chain + one retry per engine. Returns structured "engines_tried"
  + "errors" + "hint" when both empty so the agent can pivot.
- **`harness new --local` / `--workspace`** — auto-wires `[patch.crates-io]`
  to a local checkout so the scaffolded project builds before crates.io is
  populated.
- **`harness skills export` round-trip verified** — emitted SKILL.md
  validates against the agentskills.io spec and re-loads identically,
  including `metadata.harness.*`.
- **examples/personal-assistant** — full scheduling agent: calendar events
  + todo tasks + REPL mode + brief mode.
- **examples/investor-bot** — autonomous research agent over public web
  + SEC filings. Cites sources, refuses to hallucinate, always disclaims.
- **GitHub Actions CI** — `cargo check / test / fmt / clippy -D warnings`
  on ubuntu + macos.
- **123 unit / integration tests** (up from 92).

### Changed

- **`[lib] name = "harness_X"`** override on every library crate so external
  users write `use harness_core::*` instead of the auto-derived
  `harness_rs_core::*`. (`harness-rs` facade exposes itself as `harness`.)
- **19 public enums marked `#[non_exhaustive]`** — `Event`, `HookOutcome`,
  `Block`, `ToolRisk`, `Stage`, `CompactionStage`, `StopReason`, `ModelDelta`,
  `SubagentStatus`, `Severity`, `TurnRole`, `Execution`, `NotificationKind`,
  `SessionSource`, `ResourceKind`, `FixPatch`, `GuideScope`, `HarnessError`,
  `Transition`. Downstream matches need `..` from now on; new variants in
  0.0.x bumps won't break consumers.

### Fixed

- **DeepSeek `reasoning_content` round-trip** — Block::Reasoning preserved
  across turns; both OpenAiCompat and AnthropicNative re-emit on the wire.
  Without this, the second model call in a multi-turn loop returned
  `HTTP 400: reasoning_content in thinking mode must be passed back`.
- **`OpenAiCompat::build_messages`** no longer re-appends task description
  as a final user message after every tool call (was duplicating the task
  on each subsequent model call).
- **`providers::ollama`** uses Ollama's real default port `11434` (was an
  incorrect 43511).
- **`apply_patches` temp-diff filenames** include pid + nanos + atomic
  counter (was ms-only, collided on parallel patch application).
- **`patch -p1` tried first, then `-p0`** (was hardcoded `-p0`, silently
  losing git-style diffs).
- **`Skill` trait no longer marked `async_trait`** — none of its methods
  are async; the attribute was wrong and prevented `#[skill]` macro
  expansion in some edge cases.
- **Symlink escape in `harness-tools-fs`** — `resolve()` canonicalises
  after access; in-workspace symlinks pointing outside the root are now
  rejected.
- **`ReadFile` truncation visibility** — return value includes
  `truncated: bool` so the model knows it didn't see the whole file.

### Security

- **`FixPatch::RunCommand` was a silent arbitrary-code-execution surface**
  for any sensor (first- or third-party) to abuse. Default safelist + new
  `PreAutoFix` hook event close that. See `is_default_safe_fix` above.
- **`shell_read` per-program safe-args predicate** blocks
  `cargo install / publish`, `git config <k> <v>`, `find -exec / -delete /
  -ok / -okdir`, `xargs --exec`, and the language `install / run / exec`
  family on npm/pip/etc. Previously program-only allowlist.
- **`#[non_exhaustive]` everywhere relevant** — adding `SecretLeakingVariant`
  later won't compile against a downstream that does
  `match (..) { Existing => .., AlsoExisting => .. }` and miss the new arm.

## 0.0.1

Initial publish (15 of 18 crates landed at this version before the rename
to the `[lib] name = "harness_X"` scheme; users on 0.0.1 see import names
`harness_rs_core::*` etc.). Functional but does NOT contain the profile
mechanism, daemon, retry, or any of the audit fixes above. Prefer 0.0.2.

- Initial cut of `Model / Tool / Guide / Sensor / Hook / Compactor / Skill`
  traits.
- Five macros: `#[skill]` (agentskills.io-compliant) + `#[tool]` /
  `#[guide]` / `#[sensor]` / `#[hook]`.
- AgentLoop ReAct with auto-fix patch application + sensor feedback.
- DefaultCompactor with 5 stages (BudgetReduce → Snip → Microcompact →
  ContextCollapse → AutoCompact).
- HookBus over 27 lifecycle events (Allow / Deny / Inject / Mutate).
- Blueprint state machine (deterministic + agent hybrid).
- WorktreeSandbox + NullSandbox (Container / VM stubs).
- agentskills.io-spec-compliant `harness-rs-skills` (validate / list / lint
  / export).
- ToolRegistry + AgentLoop builder pattern.
- OpenAiCompat (DeepSeek / Groq / Together / Ollama / any OpenAI-shaped
  endpoint) + AnthropicNative (Messages API with content blocks).
- MockModel for deterministic tests.
- MCP stdio JSON-RPC server (initialize / ping / tools/list / tools/call).
- SessionRecorder + read_session + replay_as_mock — record any run, replay
  it deterministically offline against a fresh AgentLoop.
- harness CLI: `new`, `skills validate/list/lint/export`, `trace`,
  `mcp serve`.
- 92 unit / integration tests passing.

[Unreleased]: https://github.com/liliang-cn/harness-rs/compare/v0.0.4...HEAD
[0.0.4]:      https://github.com/liliang-cn/harness-rs/compare/v0.0.3...v0.0.4
[0.0.3]:      https://github.com/liliang-cn/harness-rs/compare/v0.0.2...v0.0.3
[0.0.2]:      https://github.com/liliang-cn/harness-rs/compare/v0.0.1...v0.0.2
[0.0.1]:      https://github.com/liliang-cn/harness-rs/releases/tag/v0.0.1
