# CAP — Computer-Aided Programming

A compact coding agent that reimplements the **core of [oh-my-pi](https://github.com/can1357/oh-my-pi)** —
its signature **hashline editing** — on top of harness-rs.

## The core idea: hashline

oh-my-pi's headline trick is editing by **content-hash anchors** instead of line
numbers. Every line of a file is shown with a short, stable anchor derived from
its *content*:

```
6a71  fn greet(name: &str) {
c3d2      println!("hi, {name}");
0f45  }
```

The model edits by quoting an anchor — `replace @c3d2 with …` — not by matching
a whole exact substring or citing a line number. Why it's better:

- **Stable under churn** — inserting a line at the top doesn't renumber anchors
  below it, and whitespace edits elsewhere never invalidate an unrelated patch.
  (This is the anti-line-number property that saves tokens.)
- **Batch-safe** — every op in one `hash_edit` call resolves against the
  original file, so a batch of inserts/deletes/replaces can't shift each other
  out of alignment.
- **Duplicate-proof** — identical lines get distinct anchors, so you can edit
  exactly the one you mean.

The applier lives in [`src/hashline.rs`](src/hashline.rs) — dependency-free,
with 8 unit tests (including the stability and duplicate-line properties).

## What's wired on harness

| oh-my-pi concept | Here |
|---|---|
| hashline read + edit | `HashRead` / `HashEdit` (`Tool`) over `hashline.rs` |
| workspace context assembly | `CapGuide` (`Guide`) — injects the file overview + workflow once |
| permission gating (preview → accept, once-and-forget) | `CapUi` (`Hook`): `y` / `N` / `a`=always-this-tool |
| streaming TUI + tool activity | same `CapUi` hook on `Event::ModelTokenDelta` |
| multi-turn session | `AgentLoop::run_with_seed_history` |
| **subagents (`task`)** — fan-out, isolated, structured report | `TaskTool` over `Subagent` — concurrent, read-only tools, one report object per subtask |
| **Hindsight memory** — situation → tools → outcome, recalled later | `harness-experience` `ExperienceRecorder` over CortexDB (semantic) or a local JSONL file |
| **LSP diagnostics** fed back after edits | `LspSensor` (`Sensor`) over a persistent LSP client (`lsp.rs`) — errors block, model self-corrects |
| **MCP tools** — plug in any external server | `harness-mcp-client`; `CAP_MCP="<cmd>"` mounts its tools into the loop |
| **skills** — reusable procedures, read + authored | `SkillCatalog` (`Guide`) + `skill_read` (`Tool`) + `skill_manage` — cross-session procedural memory at `~/.cap/skills` |
| **model routing** — planner + worker | strong `HARNESS_MODEL` drives the loop; fast `CAP_WORKER_MODEL` drives `task` subagents |

The whole agent is ~1 file of wiring — the framework supplies the loop,
streaming, tool dispatch, guides, sensors, hooks, subagents, memory, MCP, and
skills.

## The IDE-grade extensions

- **`task` — subagent fan-out.** Spawns one isolated sub-agent per subtask,
  concurrently, each with its own `World` and a read-only toolset. Results come
  back as a structured array (name / status / iters / result), not prose to
  parse. Use it for parallel investigation.
- **Hindsight memory.** Every turn is recorded as an episode (the situation, the
  *tools actually used*, and the outcome) and recalled on later runs — so a
  brand-new process can answer "have I done X before?" from memory alone.
  Backed by the shared **CortexDB** brain when `cortexdb-mcp-stdio` is running
  (semantic recall), otherwise a local `~/.cap/experience.jsonl` (keyword).
- **LSP diagnostics as a Sensor.** Opt in with `CAP_LSP="<server>"` (e.g.
  `rust-analyzer`, `gopls`). CAP keeps one **persistent** LSP session warm and,
  after every `hash_edit` / `write_file`, re-checks the touched file via
  `didChange`; **errors become blocking signals** the loop feeds back, so the
  agent fixes its own type errors before moving on.
- **MCP tools.** `CAP_MCP="<command>"` connects an external MCP server and mounts
  its tools into the loop — the whole MCP ecosystem, no code changes. Mutating
  MCP tools go through the same approval gate.
- **Skills.** Reusable procedures at `~/.cap/skills`. `SkillCatalog` lists them
  each session, `skill_read` loads a procedure on demand, and `skill_manage`
  lets the agent author new ones — procedural memory that survives across runs.
- **Model routing.** A strong **planner** drives the main loop (reasoning,
  orchestration); a fast **worker** drives the `task` fan-out subagents. Set
  `CAP_WORKER_MODEL` to the cheap model; `HARNESS_MODEL` stays the planner.
  Same endpoint/key, so you pay strong-model rates only where it matters.

```sh
HARNESS_MODEL=deepseek-v4-pro  CAP_WORKER_MODEL=deepseek-v4-flash \
CAP_LSP=gopls  CAP_MCP="cortexdb-mcp-stdio"  cargo run -p cap -- --yolo
```

## Two front-ends, one core

The crate is a **library** (`cap`) plus **two binaries** that share it — the
only thing they differ by is their UI hook:

- **`cap`** — the CLI / line REPL (streaming to stdout, `y/N/a` approval gate).
- **`cap-tui`** — a standalone **ratatui** full-screen TUI (scrolling
  conversation, live-streaming reply, tool-activity feed, input box). The agent
  runs on its own thread; a `TuiHook` bridges it to the render loop over
  channels. Runs YOLO.

```sh
# any OpenAI-compatible endpoint
export HARNESS_API_KEY=…  HARNESS_BASE_URL=…  HARNESS_MODEL=…

cargo run -p cap                                   # CLI REPL (NORMAL: confirm writes)
cargo run -p cap -- --yolo                         # no approval gate
cargo run -p cap -- "rename x to count in a.rs"    # single-shot
cargo run --bin cap-tui                            # the ratatui TUI

# sessions (both binaries)
cargo run -p cap -- --sessions                     # list stored sessions
cargo run -p cap -- -c                             # continue the latest here
cargo run -p cap -- --resume <id|path>             # resume a specific one
cargo run -p cap -- --session work "…"             # a named session
```

In `cap`'s NORMAL mode every `hash_edit` / `write_file` (and mutating MCP tool)
shows a preview and waits for `y/N/a`. `a` approves that tool for the session.
