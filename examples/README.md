# Examples

Four worked examples that exercise increasingly larger surface areas of the
framework. They're not products — they're proofs that the framework works for
real use cases. Each is a standalone Cargo binary in its own subdirectory.

| Example | What it shows | Live LLM? |
|---|---|---|
| [`deepseek-hello`](deepseek-hello/) | Smallest possible Hello-world against DeepSeek over the OpenAI-compatible adapter. ~30 lines. | yes (DeepSeek) |
| [`crate-keeper`](crate-keeper/) | Mock-model smoke test: deterministic ReAct loop without any network calls. The CI-friendly one. | no |
| [`personal-assistant`](personal-assistant/) | Scheduling agent — calendar events + todo tasks. Demonstrates `UserProfile` + `ProfileGuide`, REPL via `run_with_seed_history`, brief mode. | yes (DeepSeek) |
| [`investor-bot`](investor-bot/) | Autonomous research agent over public web + SEC filings. Multi-engine search fallback (DuckDuckGo → Bing), per-engine retry, source-citing. | yes (DeepSeek) |

## Running

All live-LLM examples read `DEEPSEEK_API_KEY` from the environment. Get one at
<https://platform.deepseek.com/>.

```bash
export DEEPSEEK_API_KEY=sk-...

# Smallest test that the wiring works.
cargo run -p deepseek-hello

# Deterministic smoke test (no API key needed).
cargo run -p crate-keeper

# Scheduling assistant — REPL mode.
cargo run -p personal-assistant -- repl

# Scheduling assistant — single brief.
cargo run -p personal-assistant -- brief

# Investor research bot — one-shot research task.
cargo run -p investor-bot -- "What is SpaceX's current valuation and when might it IPO?"

# Investor research bot — REPL mode.
cargo run -p investor-bot -- repl
```

## What each example demonstrates

### `deepseek-hello`

The minimum viable harness. Wires `OpenAiCompat` → `AgentLoop` → one tool, asks
DeepSeek a single question, prints the answer. Read this first if you want to
know what the absolute-minimum integration looks like.

### `crate-keeper`

Same shape as `deepseek-hello` but with `MockModel` instead of a real LLM. The
mock returns a scripted sequence of `(text, tool_call)` pairs, so the agent
loop runs deterministically — useful for CI and for understanding how the
loop actually iterates without network noise. Also exercises the
`#[tool]` macro and `WorktreeSandbox`.

### `personal-assistant`

Real-world scheduling agent. Things it shows:

- **`UserProfile` + `ProfileGuide`** — the agent knows the user's name,
  timezone, and locale. When asked "9:30 Vienna time next Tuesday" by a Beijing
  user, it does the timezone math correctly. The profile is populated from CLI
  flags / env vars in the example's `main.rs` — the framework provides the
  slot, the app decides the source.
- **`run_with_seed_history`** — REPL mode keeps the conversation in
  `ctx.history` (where the compactor can see it) instead of string-concatenating
  into `task.description`.
- **Two custom tools** (`add_event`, `add_task`) using the `#[tool]` macro,
  with a JSON-file backing store under `~/.harness-assistant/`.
- **Brief mode** — `personal-assistant brief` prints today's schedule and runs
  cleanly under a daemon (see `harness-rs-daemon`).

### `investor-bot`

The "hard" example — autonomous web research. Demonstrates:

- **Multi-engine search fallback** — DuckDuckGo first, Bing on failure, with
  one retry per engine. Returns a structured `engines_tried / errors / hint`
  payload when both come up empty so the agent can pivot rather than
  hallucinate.
- **`web_fetch` with retry/backoff** — survives transient network blips on
  long research runs.
- **Cited-source discipline** — system prompt forbids unsourced claims;
  every answer must link back to a fetched URL or SEC filing.
- **`current_time` tool** — the agent has no clock unless you give it one.
  Without this, "latest" / "current" / "now" answers are meaningless.
- **Note-taking tools** (`save_note`, `list_notes`) — multi-step research
  tasks need scratch state.

## Picking one to copy from

- **Want the smallest possible starting point?** → `deepseek-hello`.
- **Want to write tests for your agent without burning API credits?**
  → `crate-keeper` (MockModel pattern).
- **Building a domain-specific assistant with user context?**
  → `personal-assistant` (profile + REPL + brief).
- **Building a research agent that hits the web?** → `investor-bot`
  (multi-engine + retry + citations).

Each example's `Cargo.toml` shows exactly which `harness-rs-*` crates it
depends on — copy that as your dependency baseline.
