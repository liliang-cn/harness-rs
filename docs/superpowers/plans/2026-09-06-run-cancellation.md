# Run-Level Cancellation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a caller stop a running `AgentLoop` from outside — mid-tool, mid-generation, or between iterations — and get back an `Outcome::Cancelled` carrying the partial work, without the loop making one more model call.

**Architecture:** A `tokio_util::sync::CancellationToken` lives on `AgentLoop` (default: a token nobody cancels, so existing callers are untouched). The loop checks it at the top of every iteration, and races it against the two things that block — the model step (`complete` or `stream`) and tool dispatch — with `tokio::select!` so an in-flight HTTP request or tool future is dropped rather than awaited. Cancellation is an **outcome, not an error**: the run returns `Ok(Outcome::Cancelled { .. })`, fires a new `Event::Cancelled` then `Event::SessionEnd`, and never runs `force_final_synthesis`. `Session::turn` inherits the loop's token for free because it calls the same run path.

**Tech Stack:** Rust, tokio (`select!`, `time`), `tokio-util` 0.7 (`CancellationToken`), `futures` (`StreamExt`, `stream::unfold`), existing `MockModel` / `Hook` test fixtures.

**Why not an error variant:** `Stuck` and `BudgetExhausted` are already outcomes carrying `last_text`/`tools_called`/`usage` so a caller can salvage partial work. Cancellation is the same shape — the user pressed Esc, they still want what was done. An `Err` would throw that away.

**Repo facts the implementer needs (verified 2026-09-06 at commit `bf304a1`):**
- The loop body is `AgentLoop::run_with_seed_history` in `crates/harness-loop/src/lib.rs`; the iteration is `for iter in 0..ctx.policy.max_iters { … }` at line 1335 (after Task 2; 1290 before it). The model is called once per iteration at ~line 1425:
  ```rust
  let out = if self.streaming {
      self.complete_via_stream(&ctx, world).await?
  } else {
      self.model.complete(&ctx).await?
  };
  ```
- Tools are dispatched through `async fn dispatch_bounded(&self, action: &Action, world: &mut World) -> ToolResult` (~line 2109 after Task 2), which already wraps the future in `tokio::time::timeout(self.tool_timeout)`. It is called from two places: a parallel read-only prefetch (`if lead.len() > 1 {` at ~1615, `futures::future::join_all` at ~1628, on a cloned `World`) and the sequential per-call path (`for call in &out.tool_calls {` at ~1634; `tools_called += 1;` at ~1679).
- Early exits look like this (the stuck detector, ~line 1767) — mirror it:
  ```rust
  self.hooks.fire(&Event::SessionEnd, world);
  return Ok(Outcome::Stuck { reason, repeated: repeat_count, iters: iter + 1, last_text, tools_called, usage: total_usage });
  ```
  `last_text: Option<String>`, `tools_called: u32`, `total_usage: harness_core::Usage` are locals of the run body.
- `Event<'a>` in `crates/harness-core/src/event.rs` is `#[derive(Debug)] #[non_exhaustive]`; every variant has a line in `Event::name()` (~line 205-227). The doc comment on line 7 says "All 29 lifecycle events".
- `Outcome` is at ~line 488 of lib.rs. Its **variants** are `#[non_exhaustive]` (so new *fields* don't break `..` destructuring) but the **enum itself is not** — it is `#[derive(Debug, Clone)]` only. So adding the `Cancelled` variant makes every exhaustive `match` on `Outcome` in every *other* crate of the workspace a compile error (`harness-serve`, `harness-cli`, most `examples/*`). This was missed when the plan was written and discovered by Task 2's implementer; Task 2b below is the sweep. Matches inside `harness-loop` itself (`run_typed_with_max_iters`, `Session::turn`, `subagent.rs`, `tests/prefix_cache_live.rs`) were fixed as part of Task 2 because the crate could not compile otherwise.
- `AgentLoop::new` (~line 641) initialises every field literally, e.g. `streaming: false,`.
- Builder methods are `pub fn with_x(mut self, …) -> Self` (see `with_streaming` ~line 762).
- Integration tests live in `crates/harness-loop/tests/*.rs`; `tests/agent_loop.rs` lines 1–58 define the `tmp_workspace()` / `TestDir` / `task()` fixtures. **Copy those fixtures into the new test file** — test files are separate crates and cannot share them.
- `MockModel::new().script(MockResponse::text("x"))`, `.script(MockResponse::tool_call("name", json!({..})))`, `model.call_count()`.
- `Model` trait (`crates/harness-core/src/model.rs:108`): `async fn complete(&self,&Context)->Result<ModelOutput,ModelError>`; `async fn stream(&self,&Context)->Result<BoxStream<'static,Result<ModelDelta,ModelError>>,ModelError>` (has a default); `fn info(&self)->ModelInfo`. It is `#[async_trait]`.
- `Tool` trait (`crates/harness-core/src/tool.rs:40`): `fn name(&self)->&str; fn schema(&self)->&ToolSchema; fn risk(&self)->ToolRisk; async fn invoke(&self, args: Value, world: &mut World)->Result<ToolResult,ToolError>`. `ToolSchema { name, description, input }`. `ToolRisk::{ReadOnly, Idempotent, Destructive, Network}`. `ToolResult { ok, content, trace }`.
- `Hook` trait: `fn name(&self)->&str; fn matches(&self,&Event<'_>)->bool; fn fire(&self,&Event<'_>,&mut World)->HookOutcome` (`HookOutcome::Allow`).

**Standing rule for every task — before every commit run `cargo fmt --all`, then confirm both `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` exit 0.** CI (`.github/workflows/ci.yml`, lines ~52 and ~64) enforces both. Task 3's first commit went clippy-red on a `return` the plan itself prescribed; `cargo test` green says nothing about clippy. The code blocks in this plan were written by hand and are *not* guaranteed rustfmt-clean; copy them verbatim as instructed, then let rustfmt reflow them. Task 2's first commit went red on exactly this and needed a follow-up.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `Cargo.toml` (workspace root) | modify `[workspace.dependencies]` | pin `tokio-util` once for the workspace |
| `crates/harness-loop/Cargo.toml` | modify `[dependencies]` | pull `tokio-util` into the loop crate |
| `crates/harness-core/src/event.rs` | modify | add `Event::Cancelled`; fix the "29" doc; name mapping |
| `crates/harness-loop/src/lib.rs` | modify | `cancel` field, `with_cancellation`, `Outcome::Cancelled`, the three check/race points, skip synthesis |
| `crates/harness-loop/tests/cancellation.rs` | **create** | all behavioural tests for this feature |
| `crates/harness-context/Cargo.toml`, `crates/harness-context/src/runtime.rs` | modify | `GroupKill` guard; `TokioRunner::exec` kills the child's process group on drop (Task 3b) |
| `crates/harness-tools/src/agents.rs` | modify | `run_agent` uses the same guard on drop and timeout (Task 3b) |
| `crates/harness-tools/src/shell/background.rs` | modify | two comments: `SessionEnd` also fires on `Cancelled` (Task 3b) |
| `crates/harness-loop/src/hooks/broadcast.rs` | modify | project `Cancelled` onto the SSE feed (Task 5) |
| `crates/harness-loop/src/telemetry.rs` | modify | record `run.cancelled` inside the run span; settle the cancelled dispatch's `tool_starts` entry (Task 5) |
| `crates/harness-loop/src/lib.rs` | modify | `pub use tokio_util::sync::CancellationToken` (Task 5b) |
| `crates/harness-cli/Cargo.toml`, `crates/harness-cli/src/interrupt.rs` (**create**), `crates/harness-cli/src/main.rs` | modify/create | Ctrl-C cancels the armed run, per turn in the REPL; idle Ctrl-C exits 130 (Task 5b) |
| `CHANGELOG.md` | modify | one entry under Unreleased |

No new source files in `lib.rs`'s neighbourhood: the loop crate keeps its one-big-file convention and this change is ~80 lines of it.

---

### Task 1: The dependency and the event

**Files:**
- Modify: `Cargo.toml` — the `[workspace.dependencies]` block (tokio is at ~line 31)
- Modify: `crates/harness-loop/Cargo.toml` — `[dependencies]`
- Modify: `crates/harness-core/src/event.rs` — enum (~line 13 onward) and `name()` (~line 205-227)

- [ ] **Step 1: Write the failing test** — append to the very end of `crates/harness-core/src/event.rs`:

```rust
#[cfg(test)]
mod cancellation_event {
    use super::Event;

    // Cancellation is its own lifecycle event, not a `Stop` or an `Error`:
    // a hook that pages on `Error` must not fire when a user pressed Esc, and
    // a hook that bills on `Stop` must know this run did not finish.
    #[test]
    fn cancelled_is_a_named_lifecycle_event() {
        assert_eq!(Event::Cancelled.name(), "Cancelled");
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p harness-rs-core --lib cancelled_is_a_named_lifecycle_event`
Expected: compile error `no variant or associated item named `Cancelled` found for enum `Event``

- [ ] **Step 3: Add the dependency**

In the workspace `Cargo.toml`, inside `[workspace.dependencies]`, directly after the `tokio = { … }` line, add:

```toml
tokio-util = { version = "0.7", default-features = false }
```

In `crates/harness-loop/Cargo.toml`, inside `[dependencies]`, directly after the `tokio = { workspace = true, features = ["signal"] }` line, add:

```toml
# CancellationToken for stopping a run from outside (src/lib.rs `cancel`).
tokio-util       = { workspace = true }
```

- [ ] **Step 4: Add the variant**

In `crates/harness-core/src/event.rs`, line 7 says `All 29 lifecycle events the framework emits (DESIGN.md §10).` — and the number is wrong (the enum has 32 variants; five places in the repo disagree on the count). **Drop the number** rather than correct it: make the line `/// All lifecycle events the framework emits (DESIGN.md §10).` Do the same in `crates/harness-core/Cargo.toml`'s `description` (line 9): replace `and 29 lifecycle events` with `and the lifecycle events`. (Executed as commits `f5bd669` + `97c4b95`; the second is the code-review fix that made this decision.)

In the `Event` enum, immediately **before** the `Stop,` variant (~line 149), add:

```rust
    /// The run was stopped from outside through the loop's
    /// `CancellationToken`, before the model reached a natural end. Fires
    /// once, then `SessionEnd`. Distinct from `Stop` (the run finished) and
    /// `Error` (something broke): the work was fine, the caller ended it.
    Cancelled,
```

In `Event::name()`, immediately before the `Event::Stop => "Stop",` arm, add:

```rust
            Event::Cancelled => "Cancelled",
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p harness-rs-core --lib cancelled_is_a_named_lifecycle_event`
Expected: `test result: ok. 1 passed`

- [ ] **Step 6: Sanity-build the workspace**

Run: `cargo build --workspace`
Expected: clean, no edits needed. This was checked on 2026-09-06: no `match` on `Event` anywhere lists `Event::Stop` as an arm — every consumer uses `matches!`/`if let` or already ends in `_ =>` — so a new variant cannot make any match non-exhaustive. If this build *does* error, stop and report `BLOCKED` with the error; do not add arms on your own.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock crates/harness-loop/Cargo.toml crates/harness-core/src/event.rs
git commit -m "feat(core): Event::Cancelled, the thirtieth lifecycle event"
```

---

### Task 2: The token, the outcome, and the cheapest check

**Files:**
- Modify: `crates/harness-loop/src/lib.rs` — `use` block (top of file), `Outcome` (~488), `AgentLoop` struct (~555-600), `new` (~641), builders (~762), run body (~1330)
- Create: `crates/harness-loop/tests/cancellation.rs`

- [ ] **Step 1: Create the test file with fixtures and the first two tests**

Create `crates/harness-loop/tests/cancellation.rs`:

```rust
//! Stopping a run from outside. Every test here drives a real `AgentLoop`
//! against `MockModel` or a hand-rolled slow model; nothing is mocked at the
//! loop level, because the behaviour under test *is* the loop's.

use harness_context::default_world;
use harness_core::{Task, World};
use harness_loop::{AgentLoop, Outcome};
use harness_models::{MockModel, MockResponse};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_util::sync::CancellationToken;

// ------------------------------------------------------------------
// fixtures (copied from tests/agent_loop.rs — test crates cannot share)
// ------------------------------------------------------------------

fn tmp_workspace() -> (TestDir, World) {
    let td = TestDir::new();
    let world = default_world(td.0.clone());
    (td, world)
}

struct TestDir(PathBuf);
static TD_SEQ: AtomicU64 = AtomicU64::new(0);
impl TestDir {
    fn new() -> Self {
        let pid = std::process::id();
        let n = TD_SEQ.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("harness-cancel-test-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        TestDir(p)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn task(desc: &str) -> Task {
    Task {
        description: desc.into(),
        source: None,
        deadline: None,
    }
}

// ------------------------------------------------------------------
// 1. A token cancelled before the run starts costs nothing
// ------------------------------------------------------------------

#[tokio::test]
async fn a_token_cancelled_before_start_never_calls_the_model() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new().script(MockResponse::text("never seen"));
    let token = CancellationToken::new();
    token.cancel();

    let agent = AgentLoop::new(model).with_cancellation(token);
    let outcome = agent
        .run_with_max_iters(task("anything"), &mut world, 5)
        .await
        .unwrap();

    match outcome {
        Outcome::Cancelled { iters, tools_called, .. } => {
            assert_eq!(iters, 0, "no iteration should have started");
            assert_eq!(tools_called, 0);
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
    assert_eq!(agent.model.call_count(), 0, "a cancelled run must not spend a model call");
}

// ------------------------------------------------------------------
// 2. A token nobody cancels changes nothing
// ------------------------------------------------------------------

#[tokio::test]
async fn an_uncancelled_token_changes_nothing() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new().script(MockResponse::text("hello"));

    let outcome = AgentLoop::new(model)
        .with_cancellation(CancellationToken::new())
        .run_with_max_iters(task("say hi"), &mut world, 5)
        .await
        .unwrap();

    match outcome {
        Outcome::Done { text, iters, .. } => {
            assert_eq!(text.as_deref(), Some("hello"));
            assert_eq!(iters, 1);
        }
        other => panic!("expected Done, got {other:?}"),
    }
}
```

No `[dev-dependencies]` change is needed: `tokio-util` became a normal dependency in Task 1, and integration tests under `tests/` can use any normal dependency of the crate.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p harness-rs-loop --test cancellation`
Expected: compile errors — `no method named `with_cancellation`` and `no variant named `Cancelled``.

- [ ] **Step 3: Add the import, the outcome variant, the field, the builder**

At the top of `crates/harness-loop/src/lib.rs`, with the other `use` lines, add:

```rust
use tokio_util::sync::CancellationToken;
```

In `pub enum Outcome`, after the closing `},` of the `Stuck { … }` variant (the last one, ~line 551), add:

```rust
    /// The caller stopped the run through [`AgentLoop::with_cancellation`]
    /// before the model reached a natural end. Carries partial work like
    /// `Stuck` does, because the user who pressed Esc still wants what was
    /// done. The loop makes **no** further model call after this — not even
    /// the forced final synthesis the other early exits perform.
    #[non_exhaustive]
    Cancelled {
        /// Iterations that had *started* when the token fired. `0` means the
        /// token was already cancelled on entry.
        iters: u32,
        last_text: Option<String>,
        tools_called: u32,
        usage: harness_core::Usage,
    },
```

In `pub struct AgentLoop<M>`, directly after the `pub streaming: bool,` field (~line 578), add:

```rust
    /// Stops the run from outside. Checked at the top of every iteration and
    /// raced against the model step and every tool dispatch, so a cancel
    /// lands within one await point rather than at the next iteration
    /// boundary. Defaults to a token nobody holds, which never fires.
    pub cancel: CancellationToken,
```

In `AgentLoop::new`, directly after `streaming: false,`, add:

```rust
            cancel: CancellationToken::new(),
```

Directly after the `with_streaming` builder, add:

```rust
    /// Hand the run a token the caller can cancel. Cloning a
    /// `CancellationToken` shares it, so keep one and pass a clone here:
    ///
    /// ```ignore
    /// let token = CancellationToken::new();
    /// let agent = AgentLoop::new(model).with_cancellation(token.clone());
    /// // … later, from anywhere:
    /// token.cancel();
    /// ```
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancel = token;
        self
    }
```

- [ ] **Step 4: Add the check at the top of the iteration**

In the run body, find the line `for iter in 0..ctx.policy.max_iters {`. Insert as the **first statement inside** the loop body:

```rust
            if self.cancel.is_cancelled() {
                tracing::info!(iter, "run cancelled by caller");
                self.hooks.fire(&Event::Cancelled, world);
                self.hooks.fire(&Event::SessionEnd, world);
                return Ok(Outcome::Cancelled {
                    iters: iter,
                    last_text,
                    tools_called,
                    usage: total_usage,
                });
            }
```

`last_text`, `tools_called`, `total_usage` are the run body's existing locals (used the same way by the `Outcome::Stuck` return); `last_text` is declared at ~line 1252, above the loop.

**Also required for the crate to compile (discovered in execution, commit `4a192fe`):** four exhaustive matches on `Outcome` *inside* `harness-loop` needed a `Cancelled` arm — `run_typed_with_max_iters`'s text extraction and `Session::turn`'s reply (both `lib.rs`, fold into the existing `last_text` arm), `Subagent::run`'s report (`src/subagent.rs`, mirror the `BudgetExhausted` arm: text → `DoneWithConcerns`, none → `Blocked`), and `tests/prefix_cache_live.rs`'s usage closure. These are the minimal folds, not new behaviour.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p harness-rs-loop --test cancellation`
Expected: `test result: ok. 2 passed`

- [ ] **Step 6: Run the existing loop tests to make sure the default token is inert**

Run: `cargo test -p harness-rs-loop`
Expected: all green, same count as before plus 2.

- [ ] **Step 7: Commit**

```bash
git add crates/harness-loop/Cargo.toml crates/harness-loop/src/lib.rs crates/harness-loop/tests/cancellation.rs
git commit -m "feat(loop): a CancellationToken on AgentLoop, checked per iteration"
```

---

### Task 2b: Every consumer of `Outcome` learns to say "cancelled"

**Why this task exists.** `Outcome` is not `#[non_exhaustive]` at the enum level (only its variants are), so Task 2's new variant broke every exhaustive `match` on `Outcome` in every other crate of the workspace — 18 sites in 11 files. `cargo build --workspace --all-targets` currently fails with `error[E0004]`. Each site needs a real `Cancelled` arm. **The rule:** where a site already treats `BudgetExhausted` and `Stuck` identically through an or-pattern, fold `Cancelled` in (it binds the same four fields). Where a site distinguishes them — a status string, a label, a message — `Cancelled` gets its own arm with its own honest word. **Never** route a cancel through a wildcard `_ =>`: that hides the next variant too, and a UI that says "stuck" for a run the user stopped is lying.

**Files (all Modify):**
- `crates/harness-serve/src/service.rs` — `answer_of` (~line 466-474)
- `crates/harness-cli/src/main.rs` — JSON-mode match (~521-549), human-mode match (~562-585), REPL match (~809-830), replay summary (~1156-1169)
- `examples/ai-note/src/server.rs` — sync chat match (~1331-1357), SSE chat match (~1593-1658)
- `examples/cap/src/bin/cap.rs` — one-shot match (~270-287), REPL match (~362-373)
- `examples/cap/src/bin/cap-tui.rs` — (~288-296)
- `examples/investor-bot/src/main.rs` — (~580-620), (~719-760)
- `examples/personal-assistant/src/main.rs` — (~874-899), (~991-1010)
- `examples/eval-bench/src/main.rs` — (~135-151)
- `examples/eval-bench/src/bench_suite.rs` — outcome match (~728-743) **and** status match (~754-760)
- `examples/crate-keeper/src/main.rs` — (~150-158)
- `examples/deepseek-caps-e2e/src/main.rs` — `outcome_text` (~75-82)

- [ ] **Step 1: Confirm the failing gate**

Run: `cargo build --workspace --all-targets --message-format=short 2>&1 | grep -c E0004`
Expected: **some number ≥ 1 — it is not stable.** Cargo compiles crates in parallel and stops scheduling new work once errors appear, so how many crates get far enough to report varies between runs (observed 1, 8 and 10 on the same tree). Do not treat any particular count as pass/fail; the point of this step is only that the gate is red. The grep in Step 3 is the authoritative, complete list of sites, and Step 12's zero-error build is the pass condition.

- [ ] **Step 2: `harness-serve` — the answer helper**

In `crates/harness-serve/src/service.rs`, replace the whole `answer_of` function (its doc comment included) with:

```rust
/// Best-effort answer text from any terminal [`Outcome`] — a partial answer from
/// a budget-exhausted, stuck or cancelled run beats an empty reply.
fn answer_of(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Done { text, .. } => text.clone().unwrap_or_default(),
        Outcome::BudgetExhausted { last_text, .. } => last_text.clone().unwrap_or_default(),
        Outcome::Stuck { last_text, .. } => last_text.clone().unwrap_or_default(),
        Outcome::Cancelled { last_text, .. } => last_text.clone().unwrap_or_default(),
    }
}
```

- [ ] **Step 3: Get the full site list now that `harness-serve` compiles**

Run: `grep -rn "Outcome::Stuck" crates examples --include='*.rs' | grep -v "crates/harness-loop/"`
Expected: exactly these 18 lines (line numbers ±2):
`crates/harness-cli/src/main.rs:535`, `:574`, `:817`, `:1166`; `crates/harness-serve/src/service.rs:472`; `examples/ai-note/src/server.rs:1346`, `:1632`; `examples/cap/src/bin/cap.rs:278`, `:368`; `examples/cap/src/bin/cap-tui.rs:294`; `examples/investor-bot/src/main.rs:597`, `:736`; `examples/personal-assistant/src/main.rs:887`, `:1001`; `examples/eval-bench/src/main.rs:145`; `examples/eval-bench/src/bench_suite.rs:735`; `examples/crate-keeper/src/main.rs:154`; `examples/deepseek-caps-e2e/src/main.rs:78`. Every one of these is a match that lists `Stuck` and therefore must now list `Cancelled`. If you find a line not in this list, it still gets the same treatment — report it in your summary.

**This grep is blind to wildcard matches.** A `match outcome { Done {..} => …, BudgetExhausted {..} => …, _ => … }` compiles fine with the new variant and silently routes a cancel through `_`. Code review of Task 2 found exactly one such site in the workspace, `crates/harness-loop/src/receipt.rs`, where the wildcard zeroed a cancelled run's *signed* accounting; it is fixed in Task 2's fix commit. To confirm there are no others, also run:

```bash
grep -rln "Outcome::" crates examples --include='*.rs' | xargs grep -ln "_ =>" | xargs grep -n "match .*outcome\|match outcome\|match &outcome\|match result"
```

and inspect each hit: any `match` whose scrutinee is an `Outcome` (or `Result<Outcome, _>`) and whose arms include `_ =>` gets an explicit `Cancelled` arm (and `Stuck`, if that is also falling through). The known non-hits, so you don't re-investigate them: `crates/harness-loop/src/loop_engine/engine.rs` (a `RoundOutcome`, different type), `crates/harness-models/src/openai_compat.rs` (doc comment), `crates/harness-mcp-client/tests/in_loop.rs` (`matches!`).

- [ ] **Step 4: `harness-cli` — four sites, each distinguishes outcomes, so each gets its own arm**

(a) JSON mode. After the `Outcome::Stuck { … } => ( "stuck", … ),` arm (~line 535-548) add:

```rust
            Outcome::Cancelled {
                last_text,
                iters,
                tools_called,
                usage,
                ..
            } => (
                "cancelled",
                last_text.clone(),
                *iters,
                *tools_called,
                usage.input_tokens,
                usage.output_tokens,
            ),
```

(b) Human mode. After the `Outcome::Stuck { last_text, iters, reason, .. } => { eprintln!("(stuck after {iters} iters: {reason})"); … }` arm (~line 574-584) add:

```rust
            Outcome::Cancelled {
                last_text, iters, ..
            } => {
                eprintln!("(cancelled after {iters} iters)");
                if let Some(t) = last_text {
                    println!("{}", t.trim());
                }
            }
```

(c) `harness code` REPL. After the `Ok(Outcome::Stuck { … }) => { eprintln!("\x1b[33m(stuck after {iters} iters: {reason})\x1b[0m"); last_text.unwrap_or_default() }` arm (~line 817-825) add:

```rust
            Ok(Outcome::Cancelled {
                last_text, iters, ..
            }) => {
                eprintln!("\x1b[33m(cancelled after {iters} iters)\x1b[0m");
                last_text.unwrap_or_default()
            }
```

(d) Replay summary. After the `Outcome::Stuck { iters, reason, .. } => { println!("  outcome:       Stuck after {iters} iter(s): {reason}"); }` arm (~line 1166-1168) add:

```rust
        Outcome::Cancelled { iters, .. } => {
            println!("  outcome:       Cancelled after {iters} iter(s)");
        }
```

- [ ] **Step 5: `ai-note` — two sites, both carry a `warning` string the UI shows, so each gets its own**

(a) Sync chat (~line 1331-1357). After the `Outcome::Stuck { iters, last_text, usage, .. } => ( last_text.unwrap_or_else(|| "(stuck)".into()), iters, false, usage, ),` arm and before the closing `};`, add:

```rust
        Outcome::Cancelled {
            iters,
            last_text,
            usage,
            ..
        } => (
            last_text.unwrap_or_else(|| "(cancelled)".into()),
            iters,
            false,
            usage,
        ),
```

(b) SSE chat (~line 1593-1658). After the `Ok(Outcome::Stuck { … }) => { … "warning":"stuck" … }` arm closes (~line 1653) and before `Err(e) => {`, add — it is the `Stuck` arm with the word changed:

```rust
            Ok(Outcome::Cancelled {
                iters,
                last_text,
                usage,
                ..
            }) => {
                let reply = last_text.unwrap_or_else(|| "(cancelled)".into());
                if let Ok(db) = open_db_state(&s) {
                    let _ = db.append_chat_message(&uid, &sid, "asst", &reply, Some(iters));
                    let _ = db.insert_audit(
                        Some(&uid),
                        "chat_message",
                        Some(&sid),
                        Some(&json!({"iters": iters, "warning":"cancelled"}).to_string()),
                        usage.input_tokens as i64,
                        usage.output_tokens as i64,
                    );
                }
                let _ = tx_done.send(
                    json!({"type":"done","ok":false,"iters":iters,"reply":reply,"warning":"cancelled"}),
                );
            }
```

- [ ] **Step 6: `cap` and `cap-tui`**

(a) `cap.rs` one-shot (~line 270-287) distinguishes with a coloured line, so after the `Outcome::Stuck { … } => { eprintln!("\x1b[33m(stuck after {iters} iters: {reason})\x1b[0m"); last_text.clone().unwrap_or_default() }` arm add:

```rust
            Outcome::Cancelled {
                last_text, iters, ..
            } => {
                eprintln!("\x1b[33m(cancelled after {iters} iters)\x1b[0m");
                last_text.clone().unwrap_or_default()
            }
```

(b) `cap.rs` REPL (~line 366-368) and (c) `cap-tui.rs` (~line 292-294) both fold `BudgetExhausted` and `Stuck` into one or-pattern. In **each**, replace

```rust
            Ok(Outcome::BudgetExhausted { last_text, .. })
            | Ok(Outcome::Stuck { last_text, .. }) => last_text.unwrap_or_default(),
```
with
```rust
            Ok(Outcome::BudgetExhausted { last_text, .. })
            | Ok(Outcome::Stuck { last_text, .. })
            | Ok(Outcome::Cancelled { last_text, .. }) => last_text.unwrap_or_default(),
```

- [ ] **Step 7: `investor-bot` — two or-patterns that bind all four fields; fold**

(a) ~line 590-603: the head currently reads
```rust
        Outcome::BudgetExhausted {
            iters,
            last_text,
            tools_called,
            usage,
            ..
        }
        | Outcome::Stuck {
            iters,
            last_text,
            tools_called,
            usage,
            ..
        } => {
```
Insert a third alternative so it reads
```rust
        Outcome::BudgetExhausted {
            iters,
            last_text,
            tools_called,
            usage,
            ..
        }
        | Outcome::Stuck {
            iters,
            last_text,
            tools_called,
            usage,
            ..
        }
        | Outcome::Cancelled {
            iters,
            last_text,
            tools_called,
            usage,
            ..
        } => {
```
The body ("✗ stopped after {iters} iter(s), …") is accurate for a cancel and stays as is.

(b) ~line 729-742: identical shape wrapped in `Ok(…)`. Insert `| Ok(Outcome::Cancelled { iters, last_text, tools_called, usage, .. })` as the third alternative, formatted like its neighbours.

- [ ] **Step 8: `personal-assistant` — two sites; `Cancelled` gets its own arm at each**

**Correction (code review of the executed task).** This step originally said "fold", and the fold was executed in `bf4bc43`. It was wrong: the shared bodies print `— forced-synthesis answer (tool-less) —` and `asst (forced-synthesis)>`, a claim about *where the text came from* that holds for `BudgetExhausted` and `Stuck` (the loop runs `force_final_synthesis` and overwrites `last_text`) and is false for `Cancelled` (the loop returns `last_text` untouched and never synthesises). The fold-vs-own-arm rule must be judged on the arm's **body**, not only its pattern head — a body that asserts something variant-specific is discriminating even when the pattern looks shared. Fixed in the follow-up commit by removing `Cancelled` from both or-patterns and adding, at the one-shot site:

```rust
        Outcome::Cancelled { iters, last_text, .. } => {
            eprintln!("✗ cancelled after {iters} iteration(s)");
            // Not a synthesis: a cancelled run never gets a tool-less final
            // turn, so this is whatever the model had said when it was stopped.
            if let Some(t) = last_text {
                eprintln!("\n— last assistant message before cancelling —\n{t}");
            }
            if let Some(s) = &synth_handle {
                s.flush_pending().await;
            }
            std::process::exit(2);
        }
```

and at the REPL site:

```rust
            Ok(Outcome::Cancelled {
                iters, last_text, ..
            }) => {
                eprintln!("\nasst> ✗ cancelled after {iters} iterations.");
                if let Some(t) = last_text {
                    println!("\nasst (partial)> {t}");
                }
            }
```

`investor-bot`'s folds (Step 7) were checked against the same standard and are correct: its shared body says only "✗ stopped after …" plus a provenance-neutral "last assistant message before stopping", which is honest for all three.

*(Original, superseded instruction follows for the record.)*

(a) ~line 884-889: replace
```rust
        Outcome::BudgetExhausted {
            iters, last_text, ..
        }
        | Outcome::Stuck {
            iters, last_text, ..
        } => {
```
with
```rust
        Outcome::BudgetExhausted {
            iters, last_text, ..
        }
        | Outcome::Stuck {
            iters, last_text, ..
        }
        | Outcome::Cancelled {
            iters, last_text, ..
        } => {
```
(b) ~line 998-1003: same, with each alternative wrapped in `Ok(…)`.

- [ ] **Step 9: `eval-bench` — the runner folds; the suite gets its own status**

(a) `main.rs` ~line 139-150: add a third alternative to the or-pattern:
```rust
        | Outcome::Cancelled {
            last_text,
            iters,
            usage,
            ..
        } => (last_text.clone().unwrap_or_default(), *iters, usage.clone()),
```
(i.e. insert `| Outcome::Cancelled { last_text, iters, usage, .. }` between the `Stuck` alternative and the `=>`, formatted like the others).

(b) `bench_suite.rs`: this is a measurement tool, and a cancelled task is not a "wrong" one — the model never got to finish. After the `Ok(Ok(Outcome::Stuck { iters, usage, .. })) => { ("stuck", …) }` arm (~line 735-737) add:
```rust
        Ok(Ok(Outcome::Cancelled { iters, usage, .. })) => {
            ("cancelled", iters, usage.input_tokens, usage.output_tokens)
        }
```
and in the `let status = match (status_run, verified) { … }` block (~line 754-760), add a line **before** the final `(_, false) => "wrong",`:
```rust
        ("cancelled", false) => "cancelled",
```

- [ ] **Step 10: `crate-keeper`**

After the `Outcome::Stuck { iters, reason, .. } => { println!("\n✗ stuck after {iters} iteration(s): {reason}"); std::process::exit(2); }` arm (~line 154-157) add:
```rust
        Outcome::Cancelled { iters, .. } => {
            println!("\n✗ cancelled after {iters} iteration(s)");
            std::process::exit(2);
        }
```

- [ ] **Step 11: `deepseek-caps-e2e`**

In `outcome_text` (~line 75-82), replace
```rust
        Outcome::BudgetExhausted { last_text, .. } | Outcome::Stuck { last_text, .. } => {
            last_text.as_deref()
        }
```
with
```rust
        Outcome::BudgetExhausted { last_text, .. }
        | Outcome::Stuck { last_text, .. }
        | Outcome::Cancelled { last_text, .. } => last_text.as_deref(),
```

- [ ] **Step 12: Verify the gate passes and nothing regressed**

Run, in order:
```bash
cargo build --workspace --all-targets            # expected: clean, zero E0004
cargo clippy --workspace --all-targets -- -D warnings   # expected: clean
cargo fmt --all -- --check                       # expected: clean (run `cargo fmt --all` first if it reports your new arms)
cargo test -p harness-rs-loop 2>&1 | grep -E "^test result|FAILED"   # expected: all ok, unchanged counts
```
Then `cargo test --workspace 2>&1 | grep -E "FAILED|^test result: FAILED"`. Expected: **only** the three pre-existing doctest failures in `harness-core/src/redact/mod.rs`, `harness-tools/src/datetime/mod.rs`, `harness-tools/src/browser/policy.rs` (they fail on `main` too and are being fixed separately). Anything else failing is yours.

- [ ] **Step 13: Commit**

```bash
git add crates/harness-serve/src/service.rs crates/harness-cli/src/main.rs examples/ai-note/src/server.rs examples/cap/src/bin/cap.rs examples/cap/src/bin/cap-tui.rs examples/investor-bot/src/main.rs examples/personal-assistant/src/main.rs examples/eval-bench/src/main.rs examples/eval-bench/src/bench_suite.rs examples/crate-keeper/src/main.rs examples/deepseek-caps-e2e/src/main.rs
git commit -m "feat: every consumer of Outcome says \"cancelled\" rather than failing to build"
```

---

### Task 3: A cancel during a tool call does not wait for the tool

**Files:**
- Modify: `crates/harness-loop/src/lib.rs` — `dispatch_bounded` (~2109), the sequential tool loop (~1634-1680), the prefetch block (~1615-1631)
- Modify: `crates/harness-loop/tests/cancellation.rs`

- [ ] **Step 1: Add a slow tool and two tests**

Append to `crates/harness-loop/tests/cancellation.rs`:

```rust
// ------------------------------------------------------------------
// 3. A cancel during a tool call abandons the tool
// ------------------------------------------------------------------

use async_trait::async_trait;
use harness_core::{ToolError, ToolResult, ToolRisk, ToolSchema};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A tool that takes ten seconds. If cancellation waits for it, the test
/// below takes ten seconds and fails its elapsed-time assertion.
struct SlowTool {
    schema: ToolSchema,
}
impl SlowTool {
    fn new() -> Self {
        Self {
            schema: ToolSchema {
                name: "slow".into(),
                description: "sleeps for ten seconds".into(),
                input: json!({"type": "object", "properties": {}}),
            },
        }
    }
}
#[async_trait]
impl harness_core::Tool for SlowTool {
    fn name(&self) -> &str {
        "slow"
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Idempotent
    }
    async fn invoke(&self, _args: serde_json::Value, _world: &mut World) -> Result<ToolResult, ToolError> {
        tokio::time::sleep(Duration::from_secs(10)).await;
        Ok(ToolResult { ok: true, content: json!("slept"), trace: None })
    }
}

#[tokio::test]
async fn cancelling_during_a_slow_tool_returns_promptly() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new()
        .script(MockResponse::tool_call("slow", json!({})))
        .script(MockResponse::text("unreachable"));
    let token = CancellationToken::new();
    let fire = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        fire.cancel();
    });

    let started = Instant::now();
    let agent = AgentLoop::new(model)
        .with_tool(Arc::new(SlowTool::new()))
        .with_cancellation(token);
    let outcome = agent
        .run_with_max_iters(task("call the slow tool"), &mut world, 5)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    match outcome {
        Outcome::Cancelled { iters, tools_called, .. } => {
            assert_eq!(iters, 1, "the first iteration had started");
            assert_eq!(tools_called, 1, "the tool was dispatched before the cancel");
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "cancel must drop the in-flight tool, not await it: took {elapsed:?}"
    );
}

// ------------------------------------------------------------------
// 4. No model call after a cancel — not even the final synthesis
// ------------------------------------------------------------------

#[tokio::test]
async fn cancellation_does_not_force_a_final_synthesis() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new()
        .script(MockResponse::tool_call("slow", json!({})))
        .script(MockResponse::text("a synthesis that must never be requested"));
    let token = CancellationToken::new();
    let fire = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        fire.cancel();
    });

    let agent = AgentLoop::new(model)
        .with_tool(Arc::new(SlowTool::new()))
        .with_cancellation(token);
    let _ = agent
        .run_with_max_iters(task("call the slow tool"), &mut world, 5)
        .await
        .unwrap();

    // Exactly one call: the turn that asked for the tool. `BudgetExhausted`
    // and the monotony exit both call the model once more to write a final
    // answer; a cancel is the user saying stop, and stop means stop.
    assert_eq!(agent.model.call_count(), 1);
}

// ------------------------------------------------------------------
// 5. The partial text survives a cancel
// ------------------------------------------------------------------

/// `Cancelled` carries `last_text` because the user who pressed Esc still
/// wants what was done. No other test destructures it — they all write
/// `..` — so nothing else would notice if the field were always `None`.
#[tokio::test]
async fn partial_text_survives_a_cancel() {
    let (_td, mut world) = tmp_workspace();
    // Text and a tool call in the same turn: the loop records the text,
    // then dispatches the (slow) tool, during which the cancel lands.
    let model = MockModel::new()
        .script(MockResponse::tool_call("slow", json!({})).with_text("so far so good"))
        .script(MockResponse::text("unreachable"));
    let token = CancellationToken::new();
    let fire = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        fire.cancel();
    });

    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(SlowTool::new()))
        .with_cancellation(token)
        .run_with_max_iters(task("call the slow tool"), &mut world, 5)
        .await
        .unwrap();

    match outcome {
        Outcome::Cancelled { last_text, .. } => {
            assert_eq!(last_text.as_deref(), Some("so far so good"));
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
}
```

`async-trait` is already in `[dev-dependencies]` of `crates/harness-loop/Cargo.toml` (line 73); nothing to add. `MockResponse::with_text` exists (`crates/harness-models/src/mock.rs:81`).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p harness-rs-loop --test cancellation cancelling_during_a_slow_tool_returns_promptly`
Expected: FAIL on the elapsed assertion (`took 10.0…s`) — the tool ran to completion, the cancel was only noticed at the next iteration boundary. If instead it reports `Done`, the cancel landed after the loop finished; either way it is a failure of the same cause.

- [ ] **Step 3: Race tool dispatch against the token**

Replace the whole body of `dispatch_bounded` with:

```rust
    async fn dispatch_bounded(&self, action: &Action, world: &mut World) -> ToolResult {
        let fut = self.tools.dispatch(action, world);
        let bounded = async {
            match self.tool_timeout {
                Some(deadline) => match tokio::time::timeout(deadline, fut).await {
                    Ok(r) => r,
                    Err(_) => {
                        tracing::warn!(
                            target: "harness.telemetry",
                            event = "tool.deadline",
                            "gen_ai.tool.name" = %action.tool,
                            seconds = deadline.as_secs(),
                        );
                        Ok(ToolResult {
                            ok: false,
                            content: serde_json::json!({
                                "error": format!(
                                    "tool call exceeded its {}s deadline and was cancelled; \
                                     the operation may be too broad — narrow it or try a \
                                     different approach",
                                    deadline.as_secs()
                                ),
                                "timeout": true,
                            }),
                            trace: None,
                        })
                    }
                },
                None => fut.await,
            }
        };
        // `biased` so that a token already cancelled wins even when the tool
        // future is also ready — the caller said stop, and a result produced
        // after that would be work the run is about to throw away anyway.
        let dispatched = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                tracing::info!("gen_ai.tool.name" = %action.tool, "tool call dropped: run cancelled");
                return ToolResult {
                    ok: false,
                    content: serde_json::json!({"error": "run cancelled", "cancelled": true}),
                    trace: None,
                };
            }
            r = bounded => r,
        };
        dispatched.unwrap_or_else(|e| ToolResult {
            ok: false,
            content: serde_json::json!({"error": e.to_string()}),
            trace: None,
        })
    }
```

Note the deadline arm now returns `Ok(ToolResult { .. })` (wrapped) because it lives inside the `bounded` block whose type is `Result<ToolResult, _>`; the outer `unwrap_or_else` is unchanged.

- [ ] **Step 4: Exit the iteration once a tool reports the cancel**

Find the sequential tool loop, `for call in &out.tool_calls {`. Inside it (~line 1674-1679) the result is obtained and counted like this:

```rust
                let result = if let Some(r) = prefetched.remove(&action.call_id) {
                    r
                } else {
                    self.dispatch_bounded(&action, world).await
                };
                tools_called += 1;
```

Insert the block below **directly after `tools_called += 1;`** and before the `let result = ToolResult { content: self.shape_result(…` that follows it. (On a cancel the prefetch map is empty — Step 5 makes it so — so control always reaches `dispatch_bounded`, which returns immediately with the cancelled result, and `tools_called` has already been incremented, which is what the test asserts.)

```rust
                if self.cancel.is_cancelled() {
                    tracing::info!(iter, "run cancelled during tool dispatch");
                    self.hooks.fire(&Event::Cancelled, world);
                    self.hooks.fire(&Event::SessionEnd, world);
                    return Ok(Outcome::Cancelled {
                        iters: iter + 1,
                        last_text,
                        tools_called,
                        usage: total_usage,
                    });
                }
```

`tools_called` must already have been incremented for this call at that point (the test asserts `tools_called == 1`). If the increment happens after the push, move the increment to directly after the dispatch line.

- [ ] **Step 5: Cover the parallel prefetch path the same way**

Find the prefetch block (`if lead.len() > 1 { let futs = … ; for (id, r) in futures::future::join_all(futs).await { prefetched.insert(id, r); } }`). Replace the `for (id, r) in futures::future::join_all(futs).await { … }` with:

```rust
                    let results = tokio::select! {
                        biased;
                        _ = self.cancel.cancelled() => Vec::new(),
                        rs = futures::future::join_all(futs) => rs,
                    };
                    for (id, r) in results {
                        prefetched.insert(id, r);
                    }
```

An empty `prefetched` on cancel is safe: the sequential loop that follows re-dispatches anything not prefetched, and its own cancel check (Step 4) exits on the first one.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p harness-rs-loop --test cancellation`
Expected: `test result: ok. 5 passed`, and the slow-tool tests finish in well under a second.

- [ ] **Step 7: Run the whole loop crate**

Run: `cargo test -p harness-rs-loop`
Expected: green. `tests/tool_result_cap.rs`, `tests/parallel_dispatch.rs` and `tests/stalled_turn.rs` exercise `dispatch_bounded` and the prefetch path; they are the ones that would catch a broken refactor.

- [ ] **Step 8: Commit**

```bash
cargo fmt --all && cargo fmt --all -- --check
git add crates/harness-loop/src/lib.rs crates/harness-loop/tests/cancellation.rs
git commit -m "feat(loop): a cancel drops the in-flight tool instead of awaiting it"
```

**Executed as `d9b0dc1`; review fixes in the commit after it.** Code review found: (1) *plan defect* — Step 3's `return Ok(…)` sits in tail position of the `bounded` block and fails `clippy::needless_return` under `-D warnings`, so the first commit was CI-red (Step 3 above has since been corrected to a plain tail expression, and the standing rule now names clippy); (2) the three cancel tests fired the token from a 150 ms sleep — a two-sided race whose near side can lose to Task 2's top-of-iteration check on a loaded runner — replaced by `SlowTool` signalling entry through a `tokio::sync::Notify` and a `cancel_on_entry(entered, token)` helper, so every test cancels the instant the tool is provably mid-flight (`SlowTool::new(name, risk, entered)` is the signature every later task's test uses); (3) the prefetch cancel arm had no test — added `cancelling_during_the_parallel_prefetch_returns_promptly` with two `ToolRisk::ReadOnly` slow tools and a two-call `MockResponse::tool_calls`, asserting `Cancelled { iters: 1, tools_called: 1 }` in under 2 s; (4) the stale first line of `dispatch_bounded`'s doc ("Best-effort append to the recall store", a leftover from `recall_append`) replaced with a doc that names all three exits and the drop-semantics contract; (5) a paragraph on `Outcome::Cancelled` saying a mid-flight tool's side effects may still complete unrecorded. Two further findings became **Task 3b** (a child process is orphaned on drop) and a **Task 5 amendment** (telemetry's `tool_starts` leaks an entry per cancel and `run.end` disagrees with the outcome's `tools_called`).

---

### Task 3b: A dropped child process dies with its process group

**Why this task exists.** Task 3 makes the loop *drop* a tool's future on cancel. For a shell tool that future is `TokioRunner::exec`, which awaits `tokio::process::Command::output()` with **no `kill_on_drop`** — so dropping it orphans the child, which runs to completion. `ShellRead` is `ToolRisk::ReadOnly` and permits `cargo build`/`test`/`clippy`, so a user pressing Esc during a parallel prefetch gets `Outcome::Cancelled` in microseconds while two toolchains keep running against the same target dir: cancellation that *appears* to work. This was pre-existing on the tool-deadline path; what Task 3 changes is that an everyday user action now hits it. `kill_on_drop(true)` alone is not enough — it reaches only the direct child, and `cargo` starts rustc and test binaries in the same group — so the fix is a process group plus a guard that signals the group on drop. `run_agent` in `harness-tools` already kills its group, but only on its timeout arm and inline; it moves to the same guard so drop and timeout share one mechanism.

**Files:**
- Modify: `crates/harness-context/Cargo.toml` — `[dependencies]`
- Modify: `crates/harness-context/src/runtime.rs` — `TokioRunner::exec` (~26-44), new `GroupKill`, new test module
- Modify: `crates/harness-tools/src/agents.rs` — `run_agent` (~245-293), new test in the existing `mod tests` (~659-680 has the timeout test to sit beside)
- Modify: `crates/harness-tools/src/shell/background.rs` — two comments (~24, ~753)

Package names: `crates/harness-context/` is `harness-rs-context`, `crates/harness-tools/` is `harness-rs-tools` (confirm with `grep '^name' crates/harness-context/Cargo.toml crates/harness-tools/Cargo.toml` before running the commands below). `harness-tools` already depends on `harness-context` and on `libc`; `harness-context` re-exports `runtime::*`, so the guard is reachable as `harness_context::GroupKill`.

- [ ] **Step 1: Add the dependency**

In `crates/harness-context/Cargo.toml`, inside `[dependencies]`, directly after the `tokio        = { workspace = true }` line, add:

```toml
# Process-group kill on drop (src/runtime.rs `GroupKill`). Unix only in
# practice; the crate compiles without it elsewhere via cfg(unix).
libc         = { workspace = true }
```

- [ ] **Step 2: Write the failing test**

Append to the very end of `crates/harness-context/src/runtime.rs`:

```rust
#[cfg(all(test, unix))]
mod process_group_on_drop {
    use super::TokioRunner;
    use harness_core::ProcessRunner;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Signal 0 probes without delivering; a pid that is dead and reaped fails it.
    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// `sh` is the direct child; the backgrounded `sleep` is its grandchild,
    /// and `sh` writes the grandchild's pid to a file before waiting on it.
    /// Dropping the exec future has to kill the grandchild as well —
    /// `kill_on_drop` alone reaches `sh`, and the sleep would run on for
    /// thirty seconds after the run reported itself cancelled.
    #[tokio::test]
    async fn dropping_an_exec_kills_the_whole_process_group() {
        let pidfile = std::env::temp_dir().join(format!(
            "harness-group-kill-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let script = format!("sleep 30 & echo $! > '{}'; wait", pidfile.display());
        let runner = Arc::new(TokioRunner);
        let handle = tokio::spawn({
            let runner = runner.clone();
            async move { runner.exec("sh", &["-c", script.as_str()], None).await }
        });

        // Wait until sh has started the grandchild and recorded its pid.
        let grandchild: i32 = loop {
            if let Some(p) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                break p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(alive(grandchild), "grandchild should be running before the drop");

        // Dropping the future is exactly what a cancelled run does.
        handle.abort();
        let _ = handle.await;

        // SIGKILL delivery and reaping are asynchronous; allow a moment.
        let deadline = Instant::now() + Duration::from_secs(2);
        while alive(grandchild) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = std::fs::remove_file(&pidfile);
        assert!(
            !alive(grandchild),
            "the grandchild survived the drop: the process group was not killed"
        );
    }
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test -p harness-rs-context --lib dropping_an_exec_kills_the_whole_process_group`
Expected: FAIL with `the grandchild survived the drop` after ~2 s. (A stray `sleep 30` from this run lives on for half a minute and then exits by itself; that is the bug being demonstrated.)

- [ ] **Step 4: The guard, and a runner that arms it**

In `crates/harness-context/src/runtime.rs`, directly **above** `/// Subprocess runner backed by `tokio::process::Command`.`, add:

```rust
/// Kills a child's whole process group when dropped, unless disarmed.
///
/// `kill_on_drop` reaches only the direct child. A `cargo test` or an external
/// agent starts compilers and test runners of its own, and those are what keep
/// running when a run is cancelled mid-tool: the direct child dies, its group
/// does not. The child is spawned as a group leader (`process_group(0)`), so
/// signalling `-pid` reaches everything it started. Disarm on the normal exit
/// path: a child that finished on its own may have deliberately left something
/// behind, and the runner has no business killing that.
#[cfg(unix)]
pub struct GroupKill {
    pid: Option<i32>,
}

#[cfg(unix)]
impl GroupKill {
    /// Arm for `child`, which must have been spawned with `process_group(0)`.
    pub fn arm(child: &tokio::process::Child) -> Self {
        Self {
            pid: child.id().map(|p| p as i32),
        }
    }

    /// The child exited on its own; leave whatever it left behind alone.
    pub fn disarm(mut self) {
        self.pid = None;
    }
}

#[cfg(unix)]
impl Drop for GroupKill {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            // SAFETY: a plain signal to a process group this runner created.
            // ESRCH (already gone) is the only expected failure and is fine.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}
```

Then replace the body of `TokioRunner::exec` (everything inside the `async fn exec(…) -> std::io::Result<ProcessOutput> { … }`) with:

```rust
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        // What `Command::output` sets up implicitly, made explicit because the
        // child is spawned by hand below to get its pid before it is awaited.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Dropping this future — a cancelled run, a tool deadline — must
            // stop the child, not orphan it into the background.
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let child = cmd.spawn()?;
        #[cfg(unix)]
        let guard = GroupKill::arm(&child);
        let out = child.wait_with_output().await?;
        #[cfg(unix)]
        guard.disarm();

        Ok(ProcessOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        })
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p harness-rs-context --lib dropping_an_exec_kills_the_whole_process_group`
Expected: PASS in well under a second. Then `cargo test -p harness-rs-context` — everything green (the crate's other tests use the runner for real commands; a child that exits normally is disarmed and its output is unchanged).

- [ ] **Step 6: Write the failing test for `run_agent`'s drop path**

In `crates/harness-tools/src/agents.rs`, inside the existing `#[cfg(test)] mod tests`, directly after the test `a_run_that_overruns_is_killed_and_says_so` (~line 661-680), add:

```rust
    // A cancelled run drops `run_agent`'s future mid-flight. That has to
    // reach the agent's whole process tree the same way the timeout does, or
    // the compilers it started keep running under a run that says it stopped.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dropped_run_kills_the_agents_process_group() {
        let pidfile = std::env::temp_dir().join(format!(
            "harness-agent-drop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let agent = ExternalAgent {
            name: "forker".into(),
            program: "sh".into(),
            args: vec!["-c".into(), "{prompt}".into()],
            about: String::new(),
            verdict: Verdict::ExitCode,
        };
        let prompt = format!("sleep 30 & echo $! > '{}'; wait", pidfile.display());
        let handle = tokio::spawn(async move {
            run_agent(&agent, &prompt, &std::env::temp_dir(), Duration::from_secs(30)).await
        });

        let grandchild: i32 = loop {
            if let Some(p) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                break p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let alive = |pid: i32| unsafe { libc::kill(pid, 0) == 0 };
        assert!(alive(grandchild), "grandchild should be running before the drop");

        handle.abort();
        let _ = handle.await;

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while alive(grandchild) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = std::fs::remove_file(&pidfile);
        assert!(
            !alive(grandchild),
            "the grandchild survived the drop: only the direct child was killed"
        );
    }
```

- [ ] **Step 7: Run it to verify it fails**

Run: `cargo test -p harness-rs-tools --lib a_dropped_run_kills_the_agents_process_group`
Expected: FAIL with `the grandchild survived the drop` — `kill_on_drop` killed `sh`, and the group kill lives only in the timeout arm, which a drop never reaches.

- [ ] **Step 8: Move `run_agent`'s group kill into the guard**

In `crates/harness-tools/src/agents.rs`, in `run_agent`:

(a) Delete the two lines
```rust
    #[cfg(unix)]
    let pid = child.id();
```
and in their place put
```rust
    // Armed for the whole tree the agent starts; disarmed only if the agent
    // exits on its own. Both a timeout and a dropped future (a cancelled
    // run) take the group down through the guard's `Drop`.
    #[cfg(unix)]
    let guard = harness_context::GroupKill::arm(&child);
```

(b) Replace the `let out = match tokio::time::timeout(timeout, child.wait_with_output()).await { … };` block — the whole `match`, including the timeout arm's inline `libc::kill` — with:

```rust
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(r) => {
            let out = r.map_err(|e| ToolError::Exec(format!("{} failed: {e}", agent.name)))?;
            #[cfg(unix)]
            guard.disarm();
            out
        }
        Err(_) => {
            // The child is dropped with the timed-out future; the guard is
            // dropped at this return and takes the rest of the group with it.
            return Ok(AgentRun {
                ok: false,
                answer: String::new(),
                stderr: String::new(),
                exit_code: -1,
                truncated: false,
                seconds: started.elapsed().as_secs(),
                timed_out: true,
            });
        }
    };
```

If `libc` is no longer referenced anywhere else in `agents.rs` after this, leave the dependency in `Cargo.toml` (the test uses it) and let clippy tell you about any now-unused `use`.

- [ ] **Step 9: Run both agent tests to verify they pass**

Run: `cargo test -p harness-rs-tools --lib agents::`
Expected: every test in the module passes, including `a_run_that_overruns_is_killed_and_says_so` (the timeout path now kills through the guard) and the new drop test.

- [ ] **Step 10: Two comments in `background.rs`**

`grep -n "BudgetExhausted alike" crates/harness-tools/src/shell/background.rs` finds two comment lines (~24 and ~753) saying `SessionEnd` fires "on Done / Stuck / BudgetExhausted alike". In both, change that phrase to `on Done / Stuck / BudgetExhausted / Cancelled alike`. This is load-bearing rather than cosmetic: `JobReaperHook` matches only `SessionEnd`, so the cancel exit firing it is what reaps background jobs on Esc.

- [ ] **Step 11: Verify**

```bash
cargo fmt --all && cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p harness-rs-context -p harness-rs-tools 2>&1 | grep -E "^test result|FAILED"
cargo test -p harness-rs-loop 2>&1 | grep -E "^test result|FAILED"    # untouched, still green
```

- [ ] **Step 12: Commit**

```bash
git add Cargo.lock crates/harness-context/Cargo.toml crates/harness-context/src/runtime.rs crates/harness-tools/src/agents.rs crates/harness-tools/src/shell/background.rs
git commit -m "fix(context,tools): a dropped child process dies with its process group"
```

(`Cargo.lock` gains one edge — `libc` under `harness-rs-context` — and has to travel with the manifest change or a `--locked` build breaks. The first execution omitted it and it was folded in by amend before review.)

**Executed as `8c76a1f`; review fixes in the commit after it.** Code review found, in order of weight: (1) the plan's stdin comment was **false** — tokio's `Command::output`, unlike std's, leaves stdin *inheriting* the parent's, so `stdin(null)` is a real behaviour change for every shell tool (a child could read REPL keystrokes or hang on `git commit`); it is the right change and the comment now says so, and Task 6's changelog records it under *Changed*; (2) `process_group(0)` removes terminal Ctrl-C propagation to shell children, and `harness-cli` has no Ctrl-C handler — so Ctrl-C in the REPL now kills the harness by default disposition and orphans a running `cargo build`: the same "cancellation that appears to work" this task exists to remove, relocated from Esc to Ctrl-C. That is **Task 5b**; (3) the `disarm` half of the contract had no test — added `a_child_that_exits_normally_keeps_its_detached_grandchild` (grandchild stdio redirected so it does not hold the runner's pipes open), proven meaningful by temporarily removing `disarm` and watching it fail; (4) `GroupKill` gained `spawn(&mut Command) -> (Child, GroupKill)` as the **only** way to obtain a guard, so `kill_on_drop` + `process_group(0)` cannot be forgotten at a call site — `arm` went private, the type got `#[must_use]` and `Debug`, and both call sites use it; (5) `libc` moved to `[target.'cfg(unix)'.dependencies]`; the pid-file waits are bounded and insist on `echo`'s trailing newline so a torn read can never parse a prefix as a different real pid; a comment says the guard is left armed across the `?` on purpose; the doc notes `ContainerSandbox` weakens the guarantee (recorded as a follow-up).

```bash
# (end of Step 12)
```

---

### Task 4: A cancel mid-generation drops the request

**Files:**
- Modify: `crates/harness-loop/src/lib.rs` — the model call (~1441 after Task 3's fix commit `06c9355`) and `complete_via_stream` (~2036); `dispatch_bounded` is at ~2148 and is **not** touched here
- Modify: `crates/harness-loop/tests/cancellation.rs`

- [ ] **Step 1: Add a slow streaming model and the test**

Append to `crates/harness-loop/tests/cancellation.rs`:

```rust
// ------------------------------------------------------------------
// 5. A cancel mid-stream stops generation
// ------------------------------------------------------------------

use futures::stream::BoxStream;
use harness_core::{Context, Event, Hook, HookOutcome, Model, ModelDelta, ModelError, ModelInfo, ModelOutput};
use std::sync::atomic::AtomicU32;

/// Streams one character every 100ms for five seconds, and announces the first
/// chunk through `entered` so a test can cancel the instant the stream is
/// provably mid-flight. Delegates everything that is not streaming to a
/// `MockModel` so `info()` needs no hand-built `ModelInfo`.
struct SlowStreamModel {
    inner: MockModel,
    entered: Arc<Notify>,
}
#[async_trait]
impl Model for SlowStreamModel {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        self.inner.complete(ctx).await
    }
    async fn stream(
        &self,
        _ctx: &Context,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        let entered = self.entered.clone();
        let s = futures::stream::unfold(0u32, move |i| {
            let entered = entered.clone();
            async move {
                if i >= 50 {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                // Announce as the first chunk goes out. The loop consumes it
                // and fires ModelTokenDelta within the same poll, so the
                // cancel that this triggers lands on the *next* await.
                if i == 0 {
                    entered.notify_one();
                }
                Some((Ok(ModelDelta::Text("x".into())), i + 1))
            }
        });
        Ok(Box::pin(s))
    }
    fn info(&self) -> ModelInfo {
        let mut info = self.inner.info();
        info.supports_streaming = true;
        info
    }
}

/// A non-streaming model whose `complete` takes ten seconds and announces
/// its entry. This is the loop's **default** path — `streaming` is off unless
/// a caller opts in — so a cancel that only worked mid-stream would miss most
/// real runs.
struct SlowCompleteModel {
    inner: MockModel,
    entered: Arc<Notify>,
}
#[async_trait]
impl Model for SlowCompleteModel {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        self.entered.notify_one();
        tokio::time::sleep(Duration::from_secs(10)).await;
        self.inner.complete(ctx).await
    }
    fn info(&self) -> ModelInfo {
        self.inner.info()
    }
}

/// Counts `ModelTokenDelta` events, so the test can prove the stream was
/// running when it was cut, not finished before the cancel landed.
struct DeltaCounter(Arc<AtomicU32>);
impl Hook for DeltaCounter {
    fn name(&self) -> &str {
        "delta-counter"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::ModelTokenDelta { .. })
    }
    fn fire(&self, _ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        HookOutcome::Allow
    }
}

#[tokio::test]
async fn cancelling_mid_stream_stops_generation() {
    let (_td, mut world) = tmp_workspace();
    let entered = Arc::new(Notify::new());
    let model = SlowStreamModel {
        inner: MockModel::new().script(MockResponse::text("unused")),
        entered: entered.clone(),
    };
    let deltas = Arc::new(AtomicU32::new(0));
    let token = CancellationToken::new();
    cancel_on_entry(entered, token.clone());

    let started = Instant::now();
    let outcome = AgentLoop::new(model)
        .with_streaming(true)
        .with_hook(Arc::new(DeltaCounter(deltas.clone())))
        .with_cancellation(token)
        .run_with_max_iters(task("stream something long"), &mut world, 5)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert!(matches!(outcome, Outcome::Cancelled { iters: 1, .. }), "got {outcome:?}");
    let seen = deltas.load(Ordering::SeqCst);
    assert!(seen >= 1, "the stream had started delivering before the cancel");
    assert!(seen < 50, "the stream was cut short, not drained: saw {seen} of 50");
    assert!(elapsed < Duration::from_secs(2), "cancel must drop the stream: took {elapsed:?}");
}

/// The default, non-streaming path: `complete()` is one HTTP round trip that
/// returns nothing until the whole answer exists. A cancel has to drop that
/// request, not wait for it.
#[tokio::test]
async fn cancelling_mid_completion_drops_the_request() {
    let (_td, mut world) = tmp_workspace();
    let entered = Arc::new(Notify::new());
    let model = SlowCompleteModel {
        inner: MockModel::new().script(MockResponse::text("never returned")),
        entered: entered.clone(),
    };
    let token = CancellationToken::new();
    cancel_on_entry(entered, token.clone());

    let started = Instant::now();
    let outcome = AgentLoop::new(model)
        .with_cancellation(token)
        .run_with_max_iters(task("think for a long time"), &mut world, 5)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert!(matches!(outcome, Outcome::Cancelled { iters: 1, .. }), "got {outcome:?}");
    assert!(
        elapsed < Duration::from_secs(2),
        "cancel must drop the in-flight completion: took {elapsed:?}"
    );
}
```

Check the exact builder name for adding a hook: run `grep -n "pub fn with_hook" crates/harness-loop/src/lib.rs`. If it is named differently, use that name in the test.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p harness-rs-loop --test cancellation cancelling_mid_`
Expected: **both** new tests FAIL. `cancelling_mid_stream_stops_generation` fails on `saw 50 of 50` (stream drained) or on the elapsed assertion at ~5 s — both mean the stream was awaited to the end. `cancelling_mid_completion_drops_the_request` fails on its elapsed assertion at ~10 s — the `complete()` future was awaited to the end, then Task 2's top-of-iteration check noticed the token. If either passes before Step 3, stop: the test is not exercising the model step.

- [ ] **Step 3: Race the model step against the token**

Replace the model call at ~line 1441:

```rust
            let out = if self.streaming {
                self.complete_via_stream(&ctx, world).await?
            } else {
                self.model.complete(&ctx).await?
            };
```

with:

```rust
            let Some(out) = self.model_step(&ctx, world).await? else {
                tracing::info!(iter, "run cancelled during model step");
                self.hooks.fire(&Event::Cancelled, world);
                self.hooks.fire(&Event::SessionEnd, world);
                return Ok(Outcome::Cancelled {
                    iters: iter + 1,
                    last_text,
                    tools_called,
                    usage: total_usage,
                });
            };
```

Then add this method to `impl<M: Model> AgentLoop<M>`, directly **above** `complete_via_stream`:

```rust
    /// One model call, racing the run's cancellation token.
    ///
    /// `None` means the token fired first. Dropping the un-awaited future is
    /// what cancels the underlying HTTP request (reqwest aborts on drop) or
    /// the SSE stream, so the model stops generating rather than finishing
    /// into a void. `biased` so a token already cancelled wins a tie.
    async fn model_step(
        &self,
        ctx: &Context,
        world: &mut World,
    ) -> Result<Option<ModelOutput>, HarnessError> {
        if self.streaming {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => Ok(None),
                r = self.complete_via_stream(ctx, world) => r.map(Some),
            }
        } else {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => Ok(None),
                r = self.model.complete(ctx) => r
                    .map(Some)
                    .map_err(harness_core::HarnessError::Model),
            }
        }
    }
```

If `HarnessError::Model` is not the variant the existing `?` on `self.model.complete` converted into, mirror whatever `From` impl that `?` used — run `grep -n "impl From<ModelError> for HarnessError" crates/harness-core/src/*.rs` to find it.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p harness-rs-loop --test cancellation`
Expected: `test result: ok. 8 passed`, the two new tests finishing in well under a second each (Task 3's fix round brought the file to 6; these add 2).

- [ ] **Step 5: Run the whole loop crate**

Run: `cargo test -p harness-rs-loop`
Expected: green. `tests/session_replay.rs` and `tests/telemetry.rs` hook `PreModel`/`PostModel`; they are unaffected because `model_step` fires neither — the surrounding code still does.

- [ ] **Step 6: Commit**

```bash
git add crates/harness-loop/src/lib.rs crates/harness-loop/tests/cancellation.rs
git commit -m "feat(loop): a cancel drops the in-flight model request or stream"
```

---

### Task 5: The event fires exactly once, then SessionEnd — and reaches the feed and the trace

**Files:**
- Modify: `crates/harness-loop/tests/cancellation.rs`
- Modify (only if Step 2 fails): `crates/harness-loop/src/lib.rs`
- Modify: `crates/harness-loop/src/hooks/broadcast.rs` — `project()` (~line 120-160)
- Modify: `crates/harness-loop/src/telemetry.rs` — the `match ev` in `fire()` (~line 139-340)

**Why the last two are here.** Code review of Task 1 found that firing `Event::Cancelled` is not enough on its own: two consumers drop it on the floor. `BroadcastHook::project()` ends in `_ => return None`, and `matches()` is `project(ev).is_some()`, so an SSE client watching a run (`harness-serve`, `examples/ai-note`) would see the feed simply stop, never told the run was cancelled. And `TelemetryHook` has no end-state arm at all — `SessionEnd` writes `run.end` regardless of how the run ended — so a cancelled run's trace is indistinguishable from a completed one. Both are three-line fixes and both belong with the event, not in a later cleanup.

- [ ] **Step 1: Add the test**

Append to `crates/harness-loop/tests/cancellation.rs`:

```rust
// ------------------------------------------------------------------
// 6. Cancelled fires once, and SessionEnd follows it
// ------------------------------------------------------------------

use std::sync::Mutex;

struct EventLog(Arc<Mutex<Vec<&'static str>>>);
impl Hook for EventLog {
    fn name(&self) -> &str {
        "event-log"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::Cancelled | Event::SessionEnd | Event::Stop | Event::Error { .. })
    }
    fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        self.0.lock().unwrap().push(ev.name());
        HookOutcome::Allow
    }
}

#[tokio::test]
async fn cancelled_fires_once_then_session_end() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new()
        .script(MockResponse::tool_call("slow", json!({})))
        .script(MockResponse::text("unreachable"));
    let log = Arc::new(Mutex::new(Vec::new()));
    let entered = Arc::new(Notify::new());
    let token = CancellationToken::new();
    cancel_on_entry(entered.clone(), token.clone());

    let _ = AgentLoop::new(model)
        .with_tool(Arc::new(SlowTool::new("slow", ToolRisk::Idempotent, entered)))
        .with_hook(Arc::new(EventLog(log.clone())))
        .with_cancellation(token)
        .run_with_max_iters(task("call the slow tool"), &mut world, 5)
        .await
        .unwrap();

    let seen = log.lock().unwrap().clone();
    assert_eq!(seen, vec!["Cancelled", "SessionEnd"], "got {seen:?}");
}
```

`ev.name()` must be `pub`; it already is (used by `SessionRecorder`). If `Event::SessionEnd` is spelled differently, `grep -n "SessionEnd" crates/harness-core/src/event.rs`.

- [ ] **Step 2: Run to verify**

Run: `cargo test -p harness-rs-loop --test cancellation cancelled_fires_once_then_session_end`
Expected: PASS on the first run — Tasks 2–4 each fire `Cancelled` then `SessionEnd` at their own exit and return immediately, so only one pair can ever be emitted. **If it fails** with a doubled `Cancelled`, one exit path is not returning after firing; find it and add the missing `return`. **If it fails** with `Stop` or `Error` present, an exit path fell through to the normal end — same fix.

- [ ] **Step 3: Commit**

```bash
git add crates/harness-loop/tests/cancellation.rs
git commit -m "test(loop): Cancelled fires once and SessionEnd follows"
```

- [ ] **Step 4: Write the failing broadcast test**

Append to `crates/harness-loop/tests/cancellation.rs`:

```rust
// ------------------------------------------------------------------
// 7. A cancel reaches the broadcast feed
// ------------------------------------------------------------------

use harness_loop::hooks::broadcast::BroadcastHook;

/// The SSE feed is how a UI watches a run. Before this, `project()` had no
/// arm for `Cancelled`, so a client saw the stream stop with no reason —
/// the exact failure the event's own doc comment warns against.
#[tokio::test]
async fn a_cancel_reaches_the_broadcast_feed() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new()
        .script(MockResponse::tool_call("slow", json!({})))
        .script(MockResponse::text("unreachable"));
    // Subscribe before the hook is handed to the loop: `subscribe` takes
    // `&self`, and the receiver outlives the hook independently.
    let hook = BroadcastHook::new(64);
    let mut rx = hook.subscribe();
    let entered = Arc::new(Notify::new());
    let token = CancellationToken::new();
    cancel_on_entry(entered.clone(), token.clone());

    let _ = AgentLoop::new(model)
        .with_tool(Arc::new(SlowTool::new("slow", ToolRisk::Idempotent, entered)))
        .with_hook(Arc::new(hook))
        .with_cancellation(token)
        .run_with_max_iters(task("call the slow tool"), &mut world, 5)
        .await
        .unwrap();

    // Drain what the feed carried. `try_recv` returns `Err(Empty)` once the
    // buffer is exhausted, which ends the loop.
    let mut names = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        names.push(ev.event);
    }
    let cancelled = names.iter().position(|n| *n == "Cancelled");
    let ended = names.iter().position(|n| *n == "SessionEnd");
    assert!(cancelled.is_some(), "Cancelled never reached the feed: {names:?}");
    assert!(ended.is_some(), "SessionEnd never reached the feed: {names:?}");
    assert!(cancelled < ended, "Cancelled must precede SessionEnd on the feed: {names:?}");
}
```

- [ ] **Step 5: Run it to verify it fails**

Run: `cargo test -p harness-rs-loop --test cancellation a_cancel_reaches_the_broadcast_feed`
Expected: FAIL with `Cancelled never reached the feed: [...]` — the list will contain `"SessionEnd"` but not `"Cancelled"`, because `project()` returns `None` for it and `matches()` therefore never lets it through.

- [ ] **Step 6: Project the event**

In `crates/harness-loop/src/hooks/broadcast.rs`, inside `fn project`, directly after the line `Event::SessionEnd => json!({}),` (~line 144), add:

```rust
        // No fields: the outcome carries the partial work, the feed only
        // needs to know the run was ended from outside rather than finished.
        Event::Cancelled => json!({}),
```

Nothing else changes: `matches()` is `project(ev).is_some()`, so this one arm is what makes the hook both match and forward the event.

- [ ] **Step 7: Run it to verify it passes**

Run: `cargo test -p harness-rs-loop --test cancellation a_cancel_reaches_the_broadcast_feed`
Expected: PASS.

- [ ] **Step 8: Write the failing telemetry test**

Append to `crates/harness-loop/tests/cancellation.rs`:

```rust
// ------------------------------------------------------------------
// 8. A cancel is recorded on the run trace
// ------------------------------------------------------------------

use std::io::Write;
use tracing_subscriber::fmt::MakeWriter;

/// A `tracing` writer that appends everything into a shared buffer.
/// (Copied from tests/telemetry.rs — test crates cannot share fixtures.)
#[derive(Clone)]
struct BufWriter(Arc<Mutex<Vec<u8>>>);
impl Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> MakeWriter<'a> for BufWriter {
    type Writer = BufWriter;
    fn make_writer(&'a self) -> BufWriter {
        self.clone()
    }
}

/// `SessionEnd` writes `run.end` however the run ended, so without its own
/// line a cancelled run's trace reads exactly like a finished one.
#[tokio::test]
async fn a_cancel_is_recorded_on_the_run_trace() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufWriter(buf.clone()))
        .with_max_level(tracing::Level::DEBUG)
        .without_time()
        .with_ansi(false)
        .finish();

    let output = {
        let _guard = tracing::subscriber::set_default(subscriber);
        let (_td, mut world) = tmp_workspace();
        let model = MockModel::new()
            .script(MockResponse::tool_call("slow", json!({})))
            .script(MockResponse::text("unreachable"));
        let entered = Arc::new(Notify::new());
        let token = CancellationToken::new();
        cancel_on_entry(entered.clone(), token.clone());

        let _ = AgentLoop::new(model)
            .with_tool(Arc::new(SlowTool::new("slow", ToolRisk::Idempotent, entered)))
            .with_hook(Arc::new(harness_loop::TelemetryHook::new()))
            .with_cancellation(token)
            .run_with_max_iters(task("call the slow tool"), &mut world, 5)
            .await
            .unwrap();

        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    };

    assert!(output.contains("run.cancelled"), "missing run.cancelled:\n{output}");
    assert!(output.contains("run.end"), "run.end must still close the trace:\n{output}");
    let cancelled = output.find("run.cancelled").unwrap();
    let ended = output.find("run.end").unwrap();
    assert!(cancelled < ended, "run.cancelled must be recorded before run.end:\n{output}");
}
```

`tracing-subscriber` is already a dev-dependency of the loop crate (used by `tests/telemetry.rs`), and `tracing` is a normal dependency, so nothing is added to `Cargo.toml`.

- [ ] **Step 9: Run it to verify it fails**

Run: `cargo test -p harness-rs-loop --test cancellation a_cancel_is_recorded_on_the_run_trace`
Expected: FAIL with `missing run.cancelled:` followed by the captured output, which will contain `run.start` and `run.end` but no `run.cancelled`.

- [ ] **Step 10: Record the cancel on the span**

In `crates/harness-loop/src/telemetry.rs`, inside `fn fire`'s `match ev`, immediately **before** the `Event::SessionEnd => {` arm (~line 312), add:

```rust
            Event::Cancelled => {
                // A cancelled dispatch fired PreToolUse but will never fire
                // PostToolUse: the loop returns before it. Settle its entry
                // here, for two reasons. `run.end` must agree with
                // `Outcome::Cancelled.tools_called`, which counts that dispatch;
                // and `tool_starts` must not grow by one per cancel across a
                // long `Session` that reuses this hook.
                let dangling: Vec<Instant> = self
                    .tool_starts
                    .lock()
                    .unwrap()
                    .drain()
                    .map(|(_, started)| started)
                    .collect();
                // The same for a model call cut off between PreModel and
                // PostModel: the wait was real and belongs in `model_ms`, so
                // `duration_ms` does not exceed `model_ms + tool_ms` by an
                // unexplained gap; the call itself never completed, so
                // `model_calls` is left alone. Clearing the two fields is
                // hygiene — the next PreModel would overwrite them anyway.
                let cut_off_model = self.model_start.lock().unwrap().take();
                *self.awaiting_first_token.lock().unwrap() = false;
                {
                    let mut t = self.totals.lock().unwrap();
                    for started in dangling {
                        t.tool_calls += 1;
                        t.tool_ms += started.elapsed().as_millis() as u64;
                    }
                    if let Some(started) = cut_off_model {
                        t.model_ms += started.elapsed().as_millis() as u64;
                    }
                }
                self.in_run(|| {
                    // Warn, not info: a cancel is the person deciding the run was
                    // not worth finishing, which is worth seeing in a trace that
                    // would otherwise look like any other run.end.
                    tracing::warn!(target: "harness.telemetry", event = "run.cancelled");
                });
            }
```

`in_run` scopes the event to the run's span (if any), the same way `BudgetWarning` does two arms above. The `tool_starts` drain answers a finding from Task 3's review: a cancel mid-tool leaves `PreToolUse`'s map entry dangling forever, and `run.end` under-reports `tool_calls` by one relative to the outcome.

Also extend the telemetry test's assertions (Step 8's `a_cancel_is_recorded_on_the_run_trace`) with one more line after the existing three, so the settlement is proven rather than assumed:

```rust
    assert!(
        output.contains("tool_calls=1"),
        "run.end must count the cancelled dispatch, as Outcome::Cancelled does:\n{output}"
    );
```

(`run.end` is emitted via `tracing::info!(… tool_calls = t.tool_calls …)`, which the fmt subscriber renders as `tool_calls=1`.)

- [ ] **Step 11: Run the tests to verify they pass**

Run: `cargo test -p harness-rs-loop --test cancellation`
Expected: `test result: ok. 11 passed` (8 after Task 4, plus the event-order, broadcast and telemetry tests here).

- [ ] **Step 12: Run the whole loop crate**

Run: `cargo test -p harness-rs-loop`
Expected: green. `tests/telemetry.rs` asserts on the shape of `run.start`/`run.end` and is unaffected by an extra line; the broadcast hook's own unit tests (in `hooks/broadcast.rs`) count projected events for specific inputs and do not include `Cancelled`.

- [ ] **Step 13: Commit**

```bash
git add crates/harness-loop/src/hooks/broadcast.rs crates/harness-loop/src/telemetry.rs crates/harness-loop/tests/cancellation.rs
git commit -m "feat(loop): a cancel reaches the broadcast feed and the run trace"
```

---

### Task 5b: Ctrl-C cancels the run in `harness-cli` instead of killing the harness

**Why this task exists.** Task 3b puts every shell tool's child in its own process group so a cancelled run can kill the whole tree. The same change means a terminal Ctrl-C no longer reaches that child by group propagation — and `harness-cli` has no Ctrl-C handler at all (the only `ctrl_c` in the workspace is the daemon's). So after 3b, Ctrl-C in `harness code` kills the harness by default disposition: no unwinding, no `GroupKill::drop`, and a running `cargo build` is orphaned. That is the "cancellation that appears to work" failure, relocated from Esc to Ctrl-C. The fix is this branch's own feature: take SIGINT, cancel the run's token, let the loop unwind and the group die. Two consumers: `harness run` (one loop, one run) and the `harness code` REPL (one loop, many turns — and a token is one-way, so Ctrl-C must cancel a **per-turn** token, and a Ctrl-C at an idle prompt should still quit).

**Files:**
- Modify: `crates/harness-loop/src/lib.rs` — one `pub use` next to the others (~line 40-56)
- Modify: `crates/harness-cli/Cargo.toml` — `tokio` gains the `signal` feature
- Create: `crates/harness-cli/src/interrupt.rs`
- Modify: `crates/harness-cli/src/main.rs` — `mod interrupt;`, `run_agent` (~441-503), `run_code` (~742-830)

Package name: `harness-rs-cli` (binary `harness`). The crate is a single `main.rs` today; `interrupt.rs` is its first module. `main` is `#[tokio::main]` (multi-thread), so a spawned watcher task keeps running while the REPL blocks in `stdin.read_line`.

- [ ] **Step 1: Re-export the token type**

In `crates/harness-loop/src/lib.rs`, directly after `pub use seal::{SealBreach, SealSet};` (~line 43), add:

```rust
/// The token `AgentLoop::with_cancellation` takes, re-exported so a caller can
/// cancel a run without depending on `tokio-util` directly.
pub use tokio_util::sync::CancellationToken;
```

`cargo build -p harness-rs-loop` → clean. (The `use tokio_util::sync::CancellationToken;` Task 2 added at the top of the file stays; a `pub use` of the same path alongside a private `use` is fine, but if rustc reports the name as already imported, replace Task 2's private `use` with this `pub use` instead.)

- [ ] **Step 2: Give the CLI signal support**

In `crates/harness-cli/Cargo.toml`, change the line `tokio          = { workspace = true }` to:

```toml
tokio          = { workspace = true, features = ["signal"] }
```

- [ ] **Step 3: Write the failing tests for the interrupt policy**

Create `crates/harness-cli/src/interrupt.rs` with **only** the tests first — the types they name do not exist yet:

```rust
//! Ctrl-C as a cancel, not a kill.
//!
//! A shell tool's child runs in its own process group so a cancelled run can
//! kill everything it started — which also means a terminal Ctrl-C no longer
//! reaches that child by group propagation. If the harness simply died on
//! SIGINT, the child would be orphaned: the failure cancellation exists to
//! prevent, moved from Esc to Ctrl-C. So the CLI takes SIGINT itself and turns
//! it into a cancel of whatever run is in flight; the loop unwinds, drops the
//! tool, and the group dies with it.

#[cfg(test)]
mod tests {
    use super::Current;

    #[test]
    fn a_ctrl_c_cancels_the_armed_run() {
        let current = Current::new();
        let token = current.arm();
        assert!(!token.is_cancelled());

        assert!(current.interrupt(), "there was a run to cancel");
        assert!(token.is_cancelled(), "the armed token must be cancelled");
        assert!(!current.interrupt(), "the same run is not cancelled twice");
    }

    #[test]
    fn a_ctrl_c_with_nothing_armed_is_reported_as_idle() {
        let current = Current::new();
        assert!(!current.interrupt());
    }

    // The REPL disarms between turns: a Ctrl-C at the prompt must not reach
    // into a run that already finished — and must not poison the next one.
    #[test]
    fn a_disarmed_token_is_left_alone() {
        let current = Current::new();
        let token = current.arm();
        current.disarm();

        assert!(!current.interrupt(), "nothing armed, so idle");
        assert!(!token.is_cancelled(), "the finished turn's token is untouched");
    }
}
```

In `crates/harness-cli/src/main.rs`, directly below the crate-level `use` block at the top (before the first `fn`/`struct`), add:

```rust
mod interrupt;
```

- [ ] **Step 4: Run to verify they fail**

Run: `cargo test -p harness-rs-cli --bin harness interrupt::`
Expected: compile error — `cannot find type `Current` in module `super``. (The crate has no `[lib]` — its `Cargo.toml` declares only the `harness` binary — so `--bin harness` is the target; `--lib` would be rejected.)

- [ ] **Step 5: Implement the policy**

Add to `crates/harness-cli/src/interrupt.rs`, **above** the `#[cfg(test)]` module:

```rust
use harness_loop::CancellationToken;
use std::sync::{Arc, Mutex};

/// The run currently entitled to be cancelled by Ctrl-C, if any.
///
/// `harness run` arms it once. The REPL arms it per turn and disarms between
/// turns, because a token is one-way: cancelling a loop-lifetime token on turn
/// three would make every later turn return `Cancelled` on entry.
#[derive(Clone, Default)]
pub struct Current(Arc<Mutex<Option<CancellationToken>>>);

impl Current {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hand Ctrl-C a fresh token for the run about to start, and return it
    /// for `AgentLoop::cancel` / `with_cancellation`.
    pub fn arm(&self) -> CancellationToken {
        let token = CancellationToken::new();
        *self.0.lock().unwrap() = Some(token.clone());
        token
    }

    /// The run is over. Until the next `arm`, Ctrl-C means "quit".
    pub fn disarm(&self) {
        self.0.lock().unwrap().take();
    }

    /// One Ctrl-C. Cancels the armed run if there is one and says whether
    /// there was; the caller decides what an idle Ctrl-C means.
    pub fn interrupt(&self) -> bool {
        match self.0.lock().unwrap().take() {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }
}

/// Watch for Ctrl-C for the life of the process. Each one cancels the armed
/// run; one that finds nothing armed calls `on_idle`. The CLI passes an exit
/// with status 130 — the shell's own code for "interrupted" — so a Ctrl-C at
/// an idle prompt still quits, and a second Ctrl-C during a cancel that is
/// slow to unwind still ends the process.
///
/// Installing the handler replaces SIGINT's default disposition for the whole
/// process, which is why `on_idle` has to exist: without it, an idle Ctrl-C
/// would be swallowed.
pub fn watch(current: Current, on_idle: impl Fn() + Send + 'static) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                // No signal support on this platform/terminal: leave the
                // default disposition in place rather than pretend.
                return;
            }
            if !current.interrupt() {
                on_idle();
            }
        }
    })
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p harness-rs-cli --lib interrupt::` (or `--bin harness`)
Expected: `test result: ok. 3 passed`.

- [ ] **Step 7: Wire `harness run`**

In `run_agent` (`main.rs` ~441), directly after `let mut world = harness_context::default_world(root);` add:

```rust
    // Ctrl-C cancels the run rather than killing the harness, so an in-flight
    // tool is dropped and the child processes it started die with their
    // group. A second Ctrl-C, or one after the run has ended, exits.
    let current = interrupt::Current::new();
    let _watcher = interrupt::watch(current.clone(), || std::process::exit(130));
```

and change `let mut loop_ = AgentLoop::new(model)` to `let mut loop_ = AgentLoop::new(model).with_cancellation(current.arm())` (keep the rest of the builder chain as is). Nothing else in `run_agent` changes: the `Outcome::Cancelled` arms Task 2b added to both its JSON and human output paths already print `"cancelled"` / `(cancelled after N iters)`.

- [ ] **Step 8: Wire the `harness code` REPL, per turn**

In `run_code` (`main.rs` ~742):

(a) Change `let loop_ = AgentLoop::new(model)` (~line 771) to `let mut loop_ = AgentLoop::new(model)` — the token is swapped per turn.

(b) Directly before `let mut seed: Vec<Turn> = Vec::new();` (~line 799) add:

```rust
    // Ctrl-C during a turn cancels that turn — a fresh token each time, since
    // a cancelled token stays cancelled and would poison every later turn.
    // Ctrl-C at the prompt, with nothing armed, quits like any other REPL.
    let current = interrupt::Current::new();
    let _watcher = interrupt::watch(current.clone(), || {
        println!();
        std::process::exit(130)
    });
```

(c) Around the turn's run — the `let outcome = loop_.run_with_seed_history(task, seed.clone(), &mut world, max_iters).await;` (~line 805-807) — arm before and disarm after:

```rust
        loop_.cancel = current.arm();
        let outcome = loop_
            .run_with_seed_history(task, seed.clone(), &mut world, max_iters)
            .await;
        current.disarm();
```

`AgentLoop::cancel` is a `pub` field (Task 2), so assignment is the intended way to give an existing loop a new token. The REPL's `Ok(Outcome::Cancelled { .. })` arm from Task 2b prints `(cancelled after N iters)` and keeps the partial reply in the conversation seed, which is the right behaviour: the user stopped the turn, they did not discard it.

- [ ] **Step 9: Verify**

```bash
cargo fmt --all && cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p harness-rs-cli 2>&1 | grep -E "^test result|FAILED"
cargo test -p harness-rs-loop 2>&1 | grep -E "^test result|FAILED"
cargo build -p harness-rs-cli
```

Then a manual smoke, which is the only honest test of a signal: in a scratch directory, `harness code --model <any configured model>` … but a live model is not available in CI, so the plan accepts the unit tests on `Current` plus this reasoning as the gate: `watch` is eleven lines whose only logic is `interrupt()` → `on_idle`, both exercised by the tests. Record in the report whether a manual smoke was possible.

- [ ] **Step 10: Commit**

```bash
git add crates/harness-loop/src/lib.rs crates/harness-cli/Cargo.toml crates/harness-cli/src/interrupt.rs crates/harness-cli/src/main.rs
git commit -m "feat(cli): Ctrl-C cancels the run instead of killing the harness"
```
(add `Cargo.lock` if the `signal` feature changed it.)

---

### Task 6: Changelog

**Files:**
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Add the entry**

Open `CHANGELOG.md`. There is **no** `## Unreleased` section yet — the file goes straight from its intro paragraph to `## 0.0.62` (line 6). Insert the following directly above the `## 0.0.62` line, leaving one blank line after the intro paragraph and one before `## 0.0.62`:

```markdown
### Breaking

- **`Outcome` gained a `Cancelled` variant.** `Outcome` is not `#[non_exhaustive]`
  at the enum level (only its variants are), so every exhaustive `match` on it
  needs a new arm. Give a cancel its own arm rather than a wildcard — a UI that
  says "stuck" for a run the user stopped is lying, and a `_ =>` will swallow
  the *next* variant too. Under Cargo's `0.0.x` rules every release is already
  incompatible, so pinned consumers are unaffected until they bump.

### Changed

- **A shell tool's child no longer inherits the harness's stdin.** `TokioRunner::exec`
  now spawns with `stdin(null)`. tokio's `Command::output` — unlike std's —
  leaves stdin inheriting the parent's, so a tool's child could read the
  user's keystrokes out from under the `harness code` REPL, or hang on
  `git commit` waiting for an editor. Nothing a tool runs should read a
  terminal; `run_agent` had already closed stdin for the same reason.
- **A shell tool's child runs in its own process group and dies with it.**
  `TokioRunner::exec` spawns through `GroupKill::spawn`, which sets
  `process_group(0)` and `kill_on_drop`, and arms a guard that `SIGKILL`s the
  group when the exec future is dropped — a cancelled run, a tool deadline.
  Before, dropping the future orphaned the child: Esc during `cargo test`
  reported `Cancelled` while the toolchain kept running. The guard is
  disarmed when the child exits on its own, so a deliberately detached
  grandchild (`nohup server &`) still survives. Consequence: a terminal
  Ctrl-C no longer reaches the child by group propagation — the CLI now
  handles Ctrl-C itself by cancelling the run (see below).

### Added

- **Ctrl-C cancels the run in `harness-cli`.** `harness run` and the
  `harness code` REPL install a `tokio::signal::ctrl_c` handler that cancels
  the loop's token instead of letting the process die. The run returns
  `Outcome::Cancelled` with its partial work, in-flight tools and model calls
  are dropped, and child processes die with their group.
- **Run-level cancellation.** `AgentLoop::with_cancellation(CancellationToken)`
  stops a run from outside. The token is checked every iteration and raced
  against the model step and every tool dispatch, so a cancel drops the
  in-flight HTTP request, SSE stream or tool future instead of awaiting it.
  The run returns `Outcome::Cancelled { iters, last_text, tools_called, usage }`
  — an outcome, not an error, because the partial work is still the caller's —
  and makes no further model call, not even the forced final synthesis the
  other early exits perform. Fires the new `Event::Cancelled` (the 30th
  lifecycle event) followed by `SessionEnd`. A `Session` inherits its loop's
  token. Default is a token nobody holds, so existing callers are unchanged.
```

- [ ] **Step 2: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs: changelog for run-level cancellation"
```

---

## Final verification (controller runs this, not a task)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three must be clean before `superpowers:finishing-a-development-branch`.

## Follow-ups found in review, deliberately out of scope for this branch

- **`harness-serve` retries a run whose answer is blank** (`crates/harness-serve/src/service.rs` ~312-320 and ~392-401): `answer_of(&outcome)` empty → `warn!("empty answer — retrying once")` → re-run the agent. A cancel before any token arrived has `last_text: None`, so it takes this path. Harmless today only because the token is loop-scoped: the retry hits the same cancelled token and returns immediately. If `harness-serve` ever gives each request its own token, "user cancels" becomes "server starts a fresh run". When that wiring is done: skip the retry on `matches!(outcome, Outcome::Cancelled { .. })`. Note also the comment at ~390 — "no token has been emitted into the stream either, so the re-run won't duplicate content" — is now only true for a cancel *before* the first chunk; after Task 4's fix a mid-stream cancel puts the streamed text in `last_text`, so `answer_of` is non-empty and the retry is not taken, but the comment's reasoning should be rewritten when that code is next touched.
- **`ai-note`'s frontend has no case for `warning: "cancelled"`** (`examples/ai-note/user-ui/src/components/chat/chat-sheet.tsx` ~252 handles only `budget_exhausted`; `"stuck"` has never had a case either). Nothing breaks — the string is passed through opaquely — but a cancelled turn shows no toast. Belongs with whatever adds a cancel button to that UI.
- **`harness-cli`'s JSON `"outcome"` string is an undocumented public contract.** A table test over `Outcome → kind` would pin the four strings; it needs the tuple `match` in `main.rs` (~506-560) extracted into a testable `fn`. Reasonable follow-up, not a blocker.
- **A nested `Subagent` does not inherit the parent's token.** `Subagent::new` builds a fresh `AgentLoop` with a fresh, never-cancelled token. The only place a subagent is started *inside* a run is a tool (`examples/cap/src/tools/task.rs:90`), and tools receive `&mut World`, not the loop — so there is no path to hand them the parent's token without putting one on `World`, which is a `harness-core` change. What happens today on a parent cancel: `dispatch_bounded` drops the `task` tool's future, so the nested loop simply stops being polled — the work stops — but the nested run's `SessionEnd` never fires, so its `JobReaperHook` never reaps background jobs it started. Decision for this branch: drop is the mechanism, documented on `dispatch_bounded`. Revisit with a `cancel` field on `World` (or a `child_token()` handed through `SubagentSpec`) when a real consumer needs nested cleanup; the other four `Subagent::new` sites (`learning` review, `loop_engine` maker/checker, `scheduler`, `orchestrator`) run outside or after a loop iteration and are not affected.
- **`background.rs` still says `SessionEnd` fires "on Done / Stuck / BudgetExhausted alike"** (`crates/harness-tools/src/shell/background.rs:24` and `:753`) — now also on `Cancelled`, and that is load-bearing: `JobReaperHook` matches only `SessionEnd`, so the cancel exit firing it is what reaps background jobs on Esc. Two comment lines; fold into Task 3b since it opens that crate.
- **`ContainerSandbox` weakens the group-kill guarantee** (`crates/harness-loop/src/sandbox.rs` ~291): every `runner.exec` there goes through `docker exec`, so `GroupKill` kills the host-side client and the process inside the container survives — `docker exec` propagates no signal. Pre-existing (nothing killed it before either); the `GroupKill` doc now says so. A real fix is `docker kill`/`docker exec … kill` on drop inside that backend.
- **A `Detached` background job dropped inside its spawn grace window is orphaned unrecorded** (`background.rs:190, 210-240`: `kill_on_drop(scope != Detached)`, and the job is inserted into the `JobTable` only after the grace `timeout(GRACE_MS, child.wait())`). A cancel landing in that window leaves a running detached child with no table entry for `shell_job_kill`, and two `pump` tasks writing its log forever. Narrow, `Detached`-only, pre-existing on the deadline path.
- **`bench_suite` credits a cancelled-but-verified trial as `resolved`** (status map puts `(_, true) => "resolved"` first). Pre-existing semantics shared with `timeout`/`error`, and unreachable today (nothing in eval-bench cancels). Revisit if the bench ever cancels on its own timeout instead of dropping the future.

**Do not merge this branch with Tasks 3 or 4 unlanded.** The rustdoc on `AgentLoop::cancel` (written in Task 2) states the token is "raced against the model step and every tool dispatch" — that is true only once Tasks 3 and 4 exist. Landing Task 2 alone would ship documentation promising mid-tool cancellation the code does not deliver.

---

## Self-review

**Spec coverage**
- Stop from outside, between iterations → Task 2 Step 4.
- Stop mid-tool, promptly → Task 3 (sequential + prefetch paths).
- Stop mid-generation, promptly, both streaming and not → Task 4.
- Outcome carries partial work → `Outcome::Cancelled` fields mirror `Stuck` (Task 2).
- No further model call, incl. no synthesis → asserted by Task 3 test 4 (`call_count() == 1`); guaranteed structurally because every cancel exit `return`s before the synthesis sites.
- `Event::Cancelled` then `SessionEnd`, once → Task 1 + Task 5.
- `Session::turn` inherits → no code needed; it calls `run_with_seed_history` on the same `AgentLoop`. Noted in changelog.
- Existing callers unchanged → Task 2 test 2 + full-crate runs in every task.

**Placeholder scan** — none. Every code step is complete; the two "if it is named differently, grep" notes are verification instructions, not gaps.

**Type consistency** — `with_cancellation(CancellationToken)`, field `cancel: CancellationToken`, `Outcome::Cancelled { iters, last_text, tools_called, usage }`, `Event::Cancelled` (unit variant), `model_step(&ctx, world) -> Result<Option<ModelOutput>, HarnessError>` are used identically across Tasks 2–5.
