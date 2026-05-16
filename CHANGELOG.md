# Changelog

All notable changes to the **harness-rs** workspace. Versioning is shared across
every `harness-rs-*` crate (workspace-level `[package].version`).

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

[Unreleased]: https://github.com/liliang-cn/harness-rs/compare/v0.0.3...HEAD
[0.0.3]:      https://github.com/liliang-cn/harness-rs/compare/v0.0.2...v0.0.3
[0.0.2]:      https://github.com/liliang-cn/harness-rs/compare/v0.0.1...v0.0.2
[0.0.1]:      https://github.com/liliang-cn/harness-rs/releases/tag/v0.0.1
